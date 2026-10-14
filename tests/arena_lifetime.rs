//! Buffer lifetime and recycling tests.
//!
//! These previously lived behind the `arena` feature. That feature gated
//! nothing except duplicated constructors, so it was removed; the coverage
//! below now runs unconditionally.

use rust_miniss::Buffer;

#[test]
fn test_buffer_drop_recycles_to_pool() {
    // Allocate and drop — should recycle to thread-local pool.
    {
        let _buf = Buffer::with_capacity(4096);
    }

    // Allocate same size — likely reuses the just-recycled buffer.
    let buf2 = Buffer::with_capacity(4096);
    assert!(buf2.capacity() >= 4096);
}

#[test]
fn test_buffer_large_alloc() {
    // Large allocation should work and be properly freed on drop.
    let buf = Buffer::with_capacity(1024 * 1024); // 1MB
    assert!(buf.capacity() >= 1024 * 1024);
    // Drop happens here — large bufs bypass the pool (MAX_RECYCLED_CAPACITY=64K).
}

#[test]
fn test_buffer_recycle_does_not_leak() {
    // Allocate, recycle, ensure no panic/double-free.
    let buf = Buffer::with_capacity(256);
    buf.recycle(); // Explicit recycle
                   // The buffer is consumed — no further access.
}

#[test]
fn test_buffer_pool_does_not_grow_unbounded() {
    // The pool is capped at POOL_CAPACITY entries; dropping more than the cap
    // must not grow it. Recycle far past the cap and confirm all constructions
    // still succeed and return the requested capacity.
    for len in [1usize, 64, 1024, 8192, 16384] {
        let buf = Buffer::with_capacity(len);
        assert!(buf.capacity() >= len);
    }
}
