use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};

const MAX_RECYCLED_CAPACITY: usize = 64 * 1024;

// Thread-local buffer pool — eliminates cross-core cache contention.
thread_local! {
    static BUFFER_POOL: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

const POOL_CAPACITY: usize = 256;

/// Pooled byte buffer used across the runtime to reduce heap allocations.
///
/// Automatically recycles the inner `Vec<u8>` to a thread-local pool on drop,
/// so subsequent allocations can reuse the memory without heap syscalls.
/// The `ManuallyDrop` wrapper allows us to move the Vec out in `Drop`.
#[derive(Debug, Default)]
pub struct Buffer {
    inner: ManuallyDrop<Vec<u8>>,
}

impl Buffer {
    fn recycle_vec(vec: Vec<u8>) {
        if vec.capacity() > MAX_RECYCLED_CAPACITY {
            return;
        }
        BUFFER_POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < POOL_CAPACITY {
                pool.push(vec);
            }
        });
    }

    fn acquire_vec(capacity: usize) -> Option<Vec<u8>> {
        BUFFER_POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            // Take the most recently recycled buffer (hot in cache).
            // If it's not large enough, just skip — heap alloc is fine.
            let buf = pool.pop()?;
            if buf.capacity() >= capacity {
                Some(buf)
            } else {
                // Return smaller buffer for reuse at its own capacity.
                if buf.capacity() >= 64 && pool.len() < POOL_CAPACITY {
                    pool.push(buf);
                }
                None
            }
        })
    }

    /// Creates a buffer with zeroed contents of the requested length.
    pub fn new_zeroed(len: usize) -> Self {
        if let Some(mut vec) = Self::acquire_vec(len) {
            vec.clear();
            vec.resize(len, 0);
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(vec![0u8; len]),
        }
    }

    /// Creates a buffer without zeroing (for kernel read destinations).
    /// SAFETY: All bytes are uninitialized; caller must ensure they are
    /// written before reading (io_uring CQE sets the length on completion).
    pub fn new_uninitialized(len: usize) -> Self {
        if let Some(mut vec) = Self::acquire_vec(len) {
            vec.clear();
            vec.resize(len, 0);
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(vec![0u8; len]),
        }
    }

    /// Creates a zero-cost empty buffer (no heap allocation).
    pub fn empty() -> Self {
        Self {
            inner: ManuallyDrop::new(Vec::new()),
        }
    }

    /// Creates an empty buffer with at least the provided capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        if let Some(mut vec) = Self::acquire_vec(capacity) {
            vec.clear();
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(Vec::with_capacity(capacity)),
        }
    }

    /// Creates a buffer and copies the provided slice into it.
    pub fn from_slice(data: &[u8]) -> Self {
        let len = data.len();
        if let Some(mut vec) = Self::acquire_vec(len) {
            vec.clear();
            vec.extend_from_slice(data);
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(data.to_vec()),
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
    /// Sets the logical length of the buffer without initializing new bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all bytes in the range from the old length
    /// to `len` have been initialized before they are read.
    pub unsafe fn set_len(&mut self, len: usize) {
        self.inner.set_len(len);
    }

    /// Consumes the buffer and recycles its storage back into the pool if possible.
    /// Consumes the buffer and recycles its storage. Drop also recycles,
    /// so this is redundant — kept for compatibility with BufferPool.
    pub fn recycle(mut self) {
        // SAFETY: ManuallyDrop allows taking the value without dropping it.
        // We forget(self) to prevent Drop from running on the already-moved value.
        let vec = unsafe { ManuallyDrop::take(&mut self.inner) };
        Self::recycle_vec(vec);
        std::mem::forget(self);
    }

    #[cfg(feature = "arena")]
    pub fn new_zeroed_arena(len: usize) -> Self {
        if let Some(mut vec) = Self::acquire_vec(len) {
            vec.clear();
            vec.resize(len, 0);
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(vec![0u8; len]),
        }
    }

    #[cfg(feature = "arena")]
    /// Creates a buffer without zeroing (for kernel read destinations).
    /// SAFETY: All bytes are uninitialized; caller must ensure they are
    /// written before reading (io_uring CQE sets the length on completion).
    /// Creates a buffer intended for kernel-written data.
    ///
    /// # Safety
    ///
    /// Callers must not read from the returned buffer until the bytes have
    /// been initialized by an I/O operation or another write.
    pub unsafe fn new_uninitialized_arena(len: usize) -> Self {
        if let Some(mut vec) = Self::acquire_vec(len) {
            vec.clear();
            vec.resize(len, 0);
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(vec![0u8; len]),
        }
    }

    #[cfg(feature = "arena")]
    pub fn with_capacity_arena(capacity: usize) -> Self {
        if let Some(mut vec) = Self::acquire_vec(capacity) {
            vec.clear();
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(Vec::with_capacity(capacity)),
        }
    }

    #[cfg(feature = "arena")]
    pub fn from_slice_arena(data: &[u8]) -> Self {
        let len = data.len();
        if let Some(mut vec) = Self::acquire_vec(len) {
            vec.clear();
            vec.extend_from_slice(data);
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(data.to_vec()),
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // Take ownership of the Vec and recycle it to the thread-local pool.
        // Without this, the buffer pool is never populated and every
        // Buffer::new_zeroed() allocates fresh memory.
        // SAFETY: ManuallyDrop lets us take the inner Vec even though
        // we have Drop. After taking, self.inner is an empty Vec.
        let vec = unsafe { ManuallyDrop::take(&mut self.inner) };
        if vec.capacity() > 0 {
            Self::recycle_vec(vec);
        }
    }
}

impl Clone for Buffer {
    fn clone(&self) -> Self {
        // ManuallyDrop doesn't impl Clone, but Vec does, so deref first.
        Self {
            inner: ManuallyDrop::new((*self.inner).clone()),
        }
    }
}

impl From<Vec<u8>> for Buffer {
    fn from(inner: Vec<u8>) -> Self {
        Self {
            inner: ManuallyDrop::new(inner),
        }
    }
}

impl From<Buffer> for Vec<u8> {
    fn from(mut buffer: Buffer) -> Self {
        // SAFETY: ManuallyDrop allows taking the value without running Drop.
        let vec = unsafe { ManuallyDrop::take(&mut buffer.inner) };
        std::mem::forget(buffer);
        vec
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
