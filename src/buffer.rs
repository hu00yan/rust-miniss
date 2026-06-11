use crossbeam_queue::ArrayQueue;
use once_cell::sync::Lazy;
use std::ops::{Deref, DerefMut};

const POOL_CAPACITY: usize = 1024;
const MAX_RECYCLED_CAPACITY: usize = 64 * 1024;

static BUFFER_POOL: Lazy<ArrayQueue<Vec<u8>>> = Lazy::new(|| ArrayQueue::new(POOL_CAPACITY));

/// Pooled byte buffer used across the runtime to reduce heap allocations.
#[derive(Debug, Default)]
pub struct Buffer {
    inner: Vec<u8>,
}

impl Buffer {
    /// Creates a buffer with zeroed contents of the requested length.
    pub fn new_zeroed(len: usize) -> Self {
        Self {
            inner: vec![0u8; len],
        }
    }

    /// Creates an empty buffer with at least the provided capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity <= MAX_RECYCLED_CAPACITY {
            if let Some(mut vec) = BUFFER_POOL.pop() {
                if vec.capacity() >= capacity {
                    vec.clear();
                    return Self { inner: vec };
                }
            }
        }

        Self {
            inner: Vec::with_capacity(capacity),
        }
    }

    /// Creates a buffer and copies the provided slice into it.
    pub fn from_slice(data: &[u8]) -> Self {
        Self {
            inner: data.to_vec(),
        }
    }

    /// Returns the underlying raw pointer for FFI operations.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.inner.as_mut_ptr()
    }

    /// Returns the length of the buffer.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns whether the buffer contains no bytes.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Resizes the buffer to the requested length, zero-filling new bytes.
    pub fn resize(&mut self, new_len: usize, value: u8) {
        self.inner.resize(new_len, value);
    }

    /// Sets the buffer length without checking.
    ///
    /// # Safety
    ///
    /// The caller must ensure `len <= capacity()` and that every byte in the
    /// new initialized range has been written before it is read.
    pub unsafe fn set_len(&mut self, len: usize) {
        self.inner.set_len(len);
    }

    /// Consumes the buffer and recycles its storage back into the pool if possible.
    pub fn recycle(self) {
        if self.inner.capacity() <= MAX_RECYCLED_CAPACITY {
            let mut vec = self.inner;
            vec.clear();
            let _ = BUFFER_POOL.push(vec);
        }
    }
}

impl Clone for Buffer {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl From<Vec<u8>> for Buffer {
    fn from(inner: Vec<u8>) -> Self {
        Self { inner }
    }
}

impl From<Buffer> for Vec<u8> {
    fn from(buffer: Buffer) -> Self {
        buffer.inner
    }
}

impl Deref for Buffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        &self.inner
    }
}

impl AsMut<[u8]> for Buffer {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.inner
    }
}

/// Simple buffer pool helpers used by legacy tests and utilities.
pub struct BufferPool;

impl BufferPool {
    pub fn get(capacity: usize) -> Buffer {
        Buffer::with_capacity(capacity)
    }

    pub fn recycle(buffer: Buffer) {
        buffer.recycle();
    }
}
