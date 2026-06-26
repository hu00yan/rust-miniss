#![cfg(feature = "arena")]

use bumpalo::Bump;
use std::cell::RefCell;

// Thread-local arena with periodic reset
thread_local! {
    static THREAD_ARENA: RefCell<Bump> = RefCell::new(Bump::new());
    static ALLOCATION_COUNT: RefCell<usize> = RefCell::new(0);
}

// Reset arena after N allocations to prevent unbounded growth
const RESET_THRESHOLD: usize = 1000;

/// Allocate bytes from the thread-local arena.
///
/// NOTE: The arena may be reset on the next allocation, so the returned
/// reference is only valid until the next call to `alloc_bytes` or `reset_arena`.
/// The caller MUST consume or copy the data before the next allocation.
pub fn alloc_bytes(len: usize) -> &'static mut [u8] {
    THREAD_ARENA.with(|arena| {
        let mut arena = arena.borrow_mut();

        // Check whether we should reset before allocating.
        // Reset invalidates all previous arena allocations, so we do it
        // *before* the current allocation, not after.
        let count = ALLOCATION_COUNT.with(|c| *c.borrow());
        if count >= RESET_THRESHOLD {
            arena.reset();
            ALLOCATION_COUNT.with(|c| *c.borrow_mut() = 0);
        } else {
            ALLOCATION_COUNT.with(|c| *c.borrow_mut() = count + 1);
        }

        let slice = arena.alloc_slice_fill_default(len);

        // SAFETY: The arena lives for the thread duration, so the reference is valid
        // as long as no reset happens. We reset only at allocation boundaries (above),
        // and the caller must consume the data before the next allocation.
        unsafe { &mut *(slice as *mut [u8]) }
    })
}

/// Manually reset the thread-local arena (call after a batch of operations)
pub fn reset_arena() {
    THREAD_ARENA.with(|arena| {
        arena.borrow_mut().reset();
    });
    ALLOCATION_COUNT.with(|c| *c.borrow_mut() = 0);
}

/// Get current allocation count (for debugging/monitoring)
pub fn allocation_count() -> usize {
    ALLOCATION_COUNT.with(|c| *c.borrow())
}
