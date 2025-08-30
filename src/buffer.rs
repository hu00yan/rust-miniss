use std::cell::RefCell;
use std::collections::VecDeque;
use std::{io::IoSlice, ops::Deref};

pub const BUFFER_SIZE: usize = 4096; // Made public
const POOL_SIZE: usize = 100;

#[derive(Debug, Clone)] // Added Debug and Clone derives
pub struct Buffer(Vec<u8>);

impl Buffer {
    /// Create a new zeroed buffer with the given capacity.
    /// This bypasses the pool, useful for specific sizes or non-pooled contexts.
    pub fn new_zeroed(capacity: usize) -> Self {
        Buffer(vec![0; capacity])
    }

    /// Recycle the buffer back to the per-CPU pool
    pub fn recycle(mut self) {
        self.0.clear(); // Clear contents for security/freshness
        self.0
            .reserve_exact(BUFFER_SIZE.saturating_sub(self.0.capacity())); // Try to ensure capacity
                                                                           // Only return to pool if its capacity matches the standard BUFFER_SIZE
        if self.0.capacity() == BUFFER_SIZE {
            CPU_BUFFER_POOL.with(|pool| {
                let mut pool = pool.borrow_mut();
                if pool.len() < POOL_SIZE {
                    pool.push_back(self.0);
                }
            });
        }
    }

    /// Create an IoSlice from this buffer
    pub fn as_io_slice(&self) -> IoSlice<'_> {
        IoSlice::new(&self.0)
    }

    /// Get the length of the buffer
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Check if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Get a mutable pointer to the buffer's data.
    /// # Safety
    /// Caller must ensure the pointer is used within the buffer's bounds
    /// and that the buffer is not mutated by other means while the pointer is active.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }

    /// Get a mutable slice of the buffer's data.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.0
    }

    /// Get a mutable slice up to the current length.
    pub fn as_mut_slice_len(&mut self) -> &mut [u8] {
        &mut self.0[..] // Fix: remove .len() to avoid immutable borrow
    }

    /// Copies data from a slice into the buffer.
    /// The buffer will be resized to match the slice's length.
    pub fn copy_from_slice(&mut self, slice: &[u8]) {
        self.0.resize(slice.len(), 0);
        self.0.copy_from_slice(slice);
    }

    /// Sets the length of the buffer.
    /// # Safety
    /// Caller must ensure that `new_len` is less than or equal to `capacity()`.
    pub unsafe fn set_len(&mut self, new_len: usize) {
        self.0.set_len(new_len);
    }

    /// Returns the capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl Deref for Buffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

pub struct BufferPool;

impl BufferPool {
    /// Get a buffer from the pool, or create a new one, with at least `capacity` bytes.
    /// The returned buffer will have its `len()` set to `capacity` and be zeroed.
    pub fn get(capacity: usize) -> Buffer {
        // If the requested capacity is 0, or larger than our standard pool size,
        // or if we can't get a suitable buffer from the pool, create a new one.
        if capacity == 0 || capacity > BUFFER_SIZE {
            return Buffer(vec![0; capacity]);
        }

        CPU_BUFFER_POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            // Try to find a buffer in the pool that has at least the required capacity
            // and resize it to the exact capacity if found.
            if let Some(mut buffer) = pool.pop_front() {
                // Ensure the buffer is large enough and zeroed up to 'capacity'
                if buffer.capacity() >= capacity {
                    buffer.resize(capacity, 0);
                    return Buffer(buffer);
                }
                // If the popped buffer is too small or has wrong capacity,
                // just drop it and create a new one. This might happen if
                // a recycled buffer was not `BUFFER_SIZE` originally.
            }
            Buffer(vec![0; capacity])
        })
    }
}

// In a thread-per-core model, we don't need Lazy initialization
thread_local! {
    static CPU_BUFFER_POOL: RefCell<VecDeque<Vec<u8>>> = RefCell::new(VecDeque::with_capacity(POOL_SIZE));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_creation() {
        let buffer = BufferPool::get(BUFFER_SIZE); // Provide capacity
        assert_eq!(buffer.len(), BUFFER_SIZE);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_buffer_deref() {
        let buffer = BufferPool::get(BUFFER_SIZE); // Provide capacity
        let slice: &[u8] = &buffer;
        assert_eq!(slice.len(), BUFFER_SIZE);
    }

    #[test]
    fn test_buffer_as_ref() {
        let buffer = BufferPool::get(BUFFER_SIZE); // Provide capacity
        let slice: &[u8] = buffer.as_ref();
        assert_eq!(slice.len(), BUFFER_SIZE);
    }

    #[test]
    fn test_buffer_io_slice() {
        let buffer = BufferPool::get(BUFFER_SIZE); // Provide capacity
        let io_slice = buffer.as_io_slice();
        assert_eq!(io_slice.len(), BUFFER_SIZE);
    }

    #[test]
    fn test_buffer_recycling() {
        // Get a buffer
        let buffer1 = BufferPool::get(BUFFER_SIZE); // Provide capacity
        let ptr1 = buffer1.as_ptr();

        // Recycle it
        buffer1.recycle();

        // Get another buffer - should reuse the recycled one
        let buffer2 = BufferPool::get(BUFFER_SIZE); // Provide capacity
        let ptr2 = buffer2.as_ptr();

        // They should be the same underlying allocation
        assert_eq!(ptr1, ptr2);
    }

    #[test]
    fn test_pool_size_limit() {
        // Fill the pool beyond its capacity
        for _ in 0..POOL_SIZE + 10 {
            let buffer = BufferPool::get(BUFFER_SIZE); // Provide capacity
            buffer.recycle();
        }

        // The pool should be capped at POOL_SIZE
        CPU_BUFFER_POOL.with(|pool| {
            let pool = pool.borrow();
            assert!(pool.len() <= POOL_SIZE);
        });
    }
}
