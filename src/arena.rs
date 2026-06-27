use bumpalo::Bump;
use std::cell::RefCell;

// Thread-local arena with periodic reset
thread_local! {
    static THREAD_ARENA: RefCell<Bump> = RefCell::new(Bump::new());
    static ALLOCATION_COUNT: RefCell<usize> = const { RefCell::new(0) };
}

// Reset arena after N allocations to prevent unbounded growth
const RESET_THRESHOLD: usize = 1000;

/// Allocate bytes from the thread-local arena and use them within a scoped callback.
///
/// The slice must not escape the callback. This keeps arena reset boundaries visible
/// and avoids handing out references that can be invalidated by the next allocation.
pub fn with_bytes<R>(len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
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
        f(slice)
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
