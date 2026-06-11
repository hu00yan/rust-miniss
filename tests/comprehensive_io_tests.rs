//! Basic regression tests covering the buffer pool helpers.

use rust_miniss::buffer::{Buffer, BufferPool};

#[test]
fn buffer_pool_recycles_small_buffers() {
    let buf1 = BufferPool::get(1024);
    assert!(buf1.capacity() >= 1024);

    // recycle and fetch again
    BufferPool::recycle(buf1);
    let buf2 = BufferPool::get(512);
    assert!(buf2.capacity() >= 512);

    BufferPool::recycle(buf2);
}

#[test]
fn buffer_new_zeroed_fills_with_zeros() {
    let buf = Buffer::new_zeroed(16);
    assert_eq!(buf.len(), 16);
    assert!(buf.iter().all(|&b| b == 0));
}

#[test]
fn buffer_from_slice_copies_data() {
    let buf = Buffer::from_slice(b"hello");
    assert_eq!(&*buf, b"hello");
}
