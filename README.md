# rust-miniss

A Rust implementation of [miniss](https://github.com/qqiangwu/miniss) - a toy version of the [Seastar](https://github.com/scylladb/seastar) framework.

## Overview

rust-miniss is a minimal async runtime that demonstrates high-performance server design principles:

- **Shared-nothing architecture**: Each CPU core runs independently
- **Lock-free communication**: Cross-CPU messaging via SPSC queues
- **Zero-copy I/O**: Using io-uring on Linux
- **Custom futures**: Understanding async internals

## Goals

1. **Educational**: Learn how async runtimes work internally
2. **Performance**: Achieve near-optimal performance for specific use cases
3. **Simplicity**: Keep the codebase small and understandable (~2000 lines)

## Architecture

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

## Building

```bash
# For Linux with io-uring support
cargo build --features io_uring

# For macOS/other platforms (fallback mode)
cargo build
```

## Examples

```rust
use rust_miniss::Runtime;

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        println!("Hello from rust-miniss!");
    });
}
```

## Status

This is a learning project and work in progress. See [DEVELOPMENT_PLAN.md](DEVELOPMENT_PLAN.md) for the roadmap.

## License

MIT OR Apache-2.0
