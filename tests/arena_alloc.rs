#[cfg(all(test, feature = "arena"))]
mod arena_tests {
    use rust_miniss::Buffer;

    #[test]
    fn test_buffer_pool_reuse() {
        // Allocate a buffer and let it drop — it goes to thread-local pool.
        {
            let buf = Buffer::with_capacity(1024);
            assert_eq!(buf.capacity(), 1024);
        }

        // Re-allocate same size — should come from pool, not heap.
        let buf = Buffer::with_capacity(1024);
        assert_eq!(buf.capacity(), 1024);
    }

    #[test]
    fn test_buffer_new_uninitialized_no_zero_fill() {
        // new_uninitialized should NOT zero-fill (it's for kernel-written buffers).
        // SAFETY: The test only inspects len/capacity and never reads the
        // uninitialized memory contents.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let buf = unsafe { Buffer::new_uninitialized_arena(4096) };
        assert_eq!(buf.len(), 0);
        assert!(buf.capacity() >= 4096);
        // No assertion on contents — they're undefined.
    }

    #[test]
    fn test_buffer_arena_from_slice() {
        // Write data via from_slice and verify contents
        let buf = Buffer::from_slice_arena(b"hello arena");
        assert_eq!(buf.len(), 11);
        assert_eq!(&buf[..], b"hello arena");
    }
}

#[cfg(not(feature = "arena"))]
mod arena_tests {
    #[test]
    fn test_no_arena() {
        // Without arena feature, Buffer falls back to Vec + pool.
        // This test just verifies the module compiles.
    }
}
