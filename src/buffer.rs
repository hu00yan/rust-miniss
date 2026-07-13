use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};

const MAX_RECYCLED_CAPACITY: usize = 64 * 1024;

// Thread-local buffer pool — eliminates cross-core cache contention.
thread_local! {
    static BUFFER_POOL: UnsafeCell<Vec<Vec<u8>>> = const { UnsafeCell::new(Vec::new()) };
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

/// Owned buffer abstraction for rent-style I/O.
///
/// The runtime converts the caller's buffer into its internal owned buffer
/// representation, submits the I/O, then converts the completed buffer back
/// into the caller's type without adding another copy.
pub trait RentBuffer: Sized + Into<Buffer> + From<Buffer> {}

impl<T> RentBuffer for T where T: Sized + Into<Buffer> + From<Buffer> {}

impl Buffer {
    fn recycle_vec(vec: Vec<u8>) {
        if vec.capacity() > MAX_RECYCLED_CAPACITY {
            return;
        }
        BUFFER_POOL.with(|pool| {
            // SAFETY: BUFFER_POOL is thread-local. Runtime accesses it only
            // from owning worker thread, so no concurrent aliases exist.
            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
            unsafe {
                let pool = &mut *pool.get();
                if pool.len() < POOL_CAPACITY {
                    pool.push(vec);
                }
            }
        });
    }

    fn acquire_vec(capacity: usize) -> Option<Vec<u8>> {
        BUFFER_POOL.with(|pool| {
            // SAFETY: BUFFER_POOL is thread-local. Runtime accesses it only
            // from owning worker thread, so no concurrent aliases exist.
            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
            unsafe {
                let pool = &mut *pool.get();
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
    /// The returned buffer has `len() == 0` and at least `len` bytes of
    /// capacity. The I/O backend passes the raw pointer and requested length to
    /// the kernel, then sets the logical length to the completed byte count.
    pub fn new_uninitialized(len: usize) -> Self {
        if let Some(mut vec) = Self::acquire_vec(len) {
            vec.clear();
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(Vec::with_capacity(len)),
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

    /// Makes the complete allocation safe to expose as a mutable byte slice.
    ///
    /// This is the containment path for APIs such as `Read::read` and
    /// `UdpSocket::recv_from`: those APIs require a `&mut [u8]`, which may not
    /// cover uninitialized spare capacity. Call [`Self::publish_read_len`] with
    /// the actual byte count after the operation completes.
    // Only the epoll/kqueue fallback calls this. Linux io_uring builds do not
    // compile that backend, but keep the containment API available to it.
    #[allow(dead_code)]
    pub(crate) fn prepare_for_safe_read(&mut self) {
        let capacity = self.inner.capacity();
        self.inner.resize(capacity, 0);
    }

    /// Publishes bytes written by a kernel read into this buffer.
    ///
    /// Returns `false` and leaves the logical length unchanged when `len`
    /// exceeds the allocation capacity. Callers must only use this after the
    /// kernel or a safe read API has initialized the reported range.
    // Only fallback backend builds call this outside unit tests.
    #[allow(dead_code)]
    pub(crate) fn publish_read_len(&mut self, len: usize) -> bool {
        if len > self.capacity() {
            return false;
        }
        // SAFETY: the caller contract above ensures the newly exposed range
        // was initialized; the capacity bound was checked immediately above.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { self.inner.set_len(len) };
        true
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
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
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
    /// Creates a buffer intended for kernel-written data.
    ///
    /// The returned buffer has `len() == 0` and at least `len` bytes of
    /// capacity. Callers must not read from it until I/O completion has set the
    /// initialized length.
    ///
    /// # Safety
    ///
    /// Callers must ensure the logical length is only increased after the new
    /// byte range has been initialized by the kernel or another writer.
    pub unsafe fn new_uninitialized_arena(len: usize) -> Self {
        if let Some(mut vec) = Self::acquire_vec(len) {
            vec.clear();
            return Self {
                inner: ManuallyDrop::new(vec),
            };
        }
        Self {
            inner: ManuallyDrop::new(Vec::with_capacity(len)),
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
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
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

impl From<Box<[u8]>> for Buffer {
    fn from(slice: Box<[u8]>) -> Self {
        Self::from(slice.into_vec())
    }
}

impl From<Buffer> for Box<[u8]> {
    fn from(buffer: Buffer) -> Self {
        Vec::<u8>::from(buffer).into_boxed_slice()
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

#[cfg(test)]
mod tests {
    use super::Buffer;
    use std::io::Read;

    #[test]
    fn safe_read_destination_is_initialized_before_slice_exposure() {
        let mut buffer = Buffer::new_uninitialized(8);
        buffer.prepare_for_safe_read();

        assert_eq!(buffer.len(), 8);
        assert_eq!(buffer.as_ref(), &[0; 8]);

        let bytes_read = (&b"abc"[..]).read(buffer.as_mut()).unwrap();
        assert_eq!(bytes_read, 3);
        assert!(buffer.publish_read_len(bytes_read));
        assert_eq!(buffer.as_ref(), b"abc");
    }

    #[test]
    fn read_length_outside_capacity_is_rejected_without_publishing() {
        let mut buffer = Buffer::new_uninitialized(4);
        assert!(!buffer.publish_read_len(5));
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn eof_publishes_an_empty_logical_buffer() {
        let mut buffer = Buffer::new_uninitialized(4);
        buffer.prepare_for_safe_read();

        let bytes_read = (&b""[..]).read(buffer.as_mut()).unwrap();
        assert_eq!(bytes_read, 0);
        assert!(buffer.publish_read_len(bytes_read));
        assert!(buffer.is_empty());
    }

    #[test]
    fn read_error_leaves_only_initialized_bytes_exposed() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _dst: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("expected read failure"))
            }
        }

        let mut buffer = Buffer::new_uninitialized(4);
        buffer.prepare_for_safe_read();
        assert!(FailingReader.read(buffer.as_mut()).is_err());
        assert_eq!(buffer.as_ref(), &[0; 4]);
    }
}
