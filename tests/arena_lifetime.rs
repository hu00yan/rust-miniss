#[cfg(all(test, feature = "arena"))]
mod arena_lifetime_tests {
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
}

#[cfg(not(feature = "arena"))]
mod arena_lifetime_tests {
    #[test]
    fn test_no_arena_fallback() {
        // Without arena, Buffer falls back to Vec-backed allocation.
        let buf = rust_miniss::Buffer::with_capacity(64);
        assert!(buf.capacity() >= 64);
    }
}
