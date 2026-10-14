//! Buffer allocation tests.
//!
//! These previously lived behind the `arena` feature and called
//! `Buffer::new_uninitialized_arena` / `Buffer::from_slice_arena`. Those
//! constructors were verbatim duplicates of `new_uninitialized` / `from_slice`
//! (both routed through the same thread-local `BUFFER_POOL`), and the `arena`
//! module they implied had zero callers. The feature is gone; the same
//! coverage is preserved against the real API.

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
    // The test only inspects len/capacity and never reads the uninitialized
    // memory contents.
    let buf = Buffer::new_uninitialized(4096);
    assert_eq!(buf.len(), 0);
    assert!(buf.capacity() >= 4096);
    // No assertion on contents — they're undefined.
}

#[test]
fn test_buffer_arena_from_slice() {
    // Write data via from_slice and verify contents
    let buf = Buffer::from_slice(b"hello arena");
    assert_eq!(buf.len(), 11);
    assert_eq!(&buf[..], b"hello arena");
}
