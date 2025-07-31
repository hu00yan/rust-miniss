use std::{ops::Deref, io::IoSlice};
use once_cell::sync::Lazy;
use std::sync::Mutex;
use std::collections::VecDeque;

const BUFFER_SIZE: usize = 4096;
const POOL_SIZE: usize = 100;

pub struct Buffer(Vec<u8>);

impl Buffer {
    /// Recycle the buffer back to the per-CPU pool
    pub fn recycle(self) {
        CPU_BUFFER_POOL.with(|pool| {
            let mut pool = pool.lock().unwrap();
            if pool.len() < POOL_SIZE {
                pool.push_back(self.0);
            }
        });
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
    pub fn get() -> Buffer {
        CPU_BUFFER_POOL.with(|pool| {
            let mut pool = pool.lock().unwrap();
            if let Some(buffer) = pool.pop_front() {
                Buffer(buffer)
            } else {
                Buffer(vec![0; BUFFER_SIZE])
            }
        })
    }
}

thread_local! {
    static CPU_BUFFER_POOL: Lazy<Mutex<VecDeque<Vec<u8>>>> = Lazy::new(|| Mutex::new(VecDeque::with_capacity(POOL_SIZE)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_creation() {
        let buffer = BufferPool::get();
        assert_eq!(buffer.len(), BUFFER_SIZE);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_buffer_deref() {
        let buffer = BufferPool::get();
        let slice: &[u8] = &*buffer;
        assert_eq!(slice.len(), BUFFER_SIZE);
    }

    #[test]
    fn test_buffer_as_ref() {
        let buffer = BufferPool::get();
        let slice: &[u8] = buffer.as_ref();
        assert_eq!(slice.len(), BUFFER_SIZE);
    }

    #[test]
    fn test_buffer_io_slice() {
        let buffer = BufferPool::get();
        let io_slice = buffer.as_io_slice();
        assert_eq!(io_slice.len(), BUFFER_SIZE);
    }

    #[test]
    fn test_buffer_recycling() {
        // Get a buffer
        let buffer1 = BufferPool::get();
        let ptr1 = buffer1.as_ptr();
        
        // Recycle it
        buffer1.recycle();
        
        // Get another buffer - should reuse the recycled one
        let buffer2 = BufferPool::get();
        let ptr2 = buffer2.as_ptr();
        
        // They should be the same underlying allocation
        assert_eq!(ptr1, ptr2);
    }

    #[test]
    fn test_pool_size_limit() {
        // Fill the pool beyond its capacity
        for _ in 0..POOL_SIZE + 10 {
            let buffer = BufferPool::get();
            buffer.recycle();
        }
        
        // The pool should be capped at POOL_SIZE
        CPU_BUFFER_POOL.with(|pool| {
            let pool = pool.lock().unwrap();
            assert!(pool.len() <= POOL_SIZE);
        });
    }
}
