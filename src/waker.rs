//! Custom Waker implementation
//!
//! This module provides a custom Waker that integrates with our executor
//! to schedule tasks when they become ready.
//!
//! # Safety
//!
//! In the thread-per-core model, all Waker operations (clone/wake/drop)
//! happen on the same thread that created the Waker. Therefore we use:
//! - `Rc` (non-atomic refcount) instead of `Arc` for the outer wrapper
//! - Raw `*const VecDeque` pointer instead of a shared locked queue to avoid
//!   atomic operations on every wake
//!
//! The VecDeque is owned by CpuCore and lives for the entire runtime —
//! strictly longer than any Waker — so the raw pointer is safe.

use std::collections::VecDeque;
use std::rc::Rc;
use std::task::{RawWaker, RawWakerVTable, Waker};

/// A task ID that uniquely identifies a task in the executor
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub u64);

impl TaskId {
    pub fn cpu_id(&self) -> usize {
        (self.0 >> 32) as usize
    }

    pub fn sequence(&self) -> u32 {
        self.0 as u32
    }

    pub fn new(core_id: usize, sequence: u32) -> Self {
        Self(((core_id as u64) << 32) | sequence as u64)
    }
}

/// Waker implementation that can reschedule tasks
///
/// Uses a raw pointer to the VecDeque to avoid atomic refcount operations.
/// The queue outlives all Wakers because it is owned by the runtime thread.
pub struct MinissWaker {
    task_id: TaskId,
    queue: *const VecDeque<TaskId>,
}

// SAFETY: MinissWaker is only accessed from the owning CPU thread.
// The raw pointer is valid for the lifetime of the runtime.
unsafe impl Send for MinissWaker {}
unsafe impl Sync for MinissWaker {}

impl MinissWaker {
    /// Create a new waker for the given task
    ///
    /// Takes a raw `*const VecDeque<TaskId>` pointer for zero-overhead access.
    /// The VecDeque is owned by the CpuCore and outlives all Wakers.
    pub fn create_waker(task_id: TaskId, queue: *const VecDeque<TaskId>) -> Waker {
        let waker = Rc::new(MinissWaker { task_id, queue });
        let raw_waker = RawWaker::new(Rc::into_raw(waker) as *const (), &VTABLE);
        // SAFETY: The RawWaker vtable reconstructs the same Rc allocation and
        // follows RawWaker ownership rules for clone/wake/drop.
        unsafe { Waker::from_raw(raw_waker) }
    }

    /// Wake the task by adding it to the run queue
    #[inline(always)]
    fn wake_impl(&self) {
        // SAFETY: The raw pointer is valid for the lifetime of this Waker.
        // The VecDeque is owned by the runtime thread and outlives all Wakers.
        // Since this is thread-per-core, the cast to *mut is safe.
        let queue = unsafe { &mut *self.queue.cast_mut() };
        queue.push_back(self.task_id);
    }
}

// Raw waker implementation — non-atomic Rc operations since all
// clone/wake/drop happen on the same thread.
static VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    // SAFETY: `data` was produced by Rc::into_raw for MinissWaker in
    // create_waker or a previous clone.
    let waker = Rc::from_raw(data as *const MinissWaker);
    let cloned = waker.clone(); // non-atomic: Cell<usize> increment
    std::mem::forget(waker); // Don't drop the original
    RawWaker::new(Rc::into_raw(cloned) as *const (), &VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    // SAFETY: `wake` consumes exactly one Rc strong ref encoded in `data`.
    let waker = Rc::from_raw(data as *const MinissWaker);
    waker.wake_impl();
    // Rc dropped here — non-atomic refcount decrement
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    // SAFETY: `wake_by_ref` borrows the MinissWaker without consuming the Rc
    // strong ref represented by `data`.
    let waker = &*(data as *const MinissWaker);
    waker.wake_impl();
}

unsafe fn waker_drop(data: *const ()) {
    // SAFETY: `drop` consumes exactly one Rc strong ref encoded in `data`.
    let _ = Rc::from_raw(data as *const MinissWaker);
    // Rc dropped here — non-atomic refcount decrement, may free MinissWaker
}

/// Create a dummy waker for simple polling scenarios
/// This is used when we don't need proper task scheduling
pub fn dummy_waker() -> Waker {
    use std::task::{RawWaker, RawWakerVTable, Waker};

    fn dummy_clone(_: *const ()) -> RawWaker {
        dummy_raw_waker()
    }
    fn dummy_wake(_: *const ()) {}
    fn dummy_wake_by_ref(_: *const ()) {}
    fn dummy_drop(_: *const ()) {}

    fn dummy_raw_waker() -> RawWaker {
        RawWaker::new(
            std::ptr::null(),
            &RawWakerVTable::new(dummy_clone, dummy_wake, dummy_wake_by_ref, dummy_drop),
        )
    }

    // SAFETY: The dummy RawWaker vtable never dereferences the null data
    // pointer and implements no-op wake/drop functions.
    unsafe { Waker::from_raw(dummy_raw_waker()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_waker_creation() {
        let mut queue = VecDeque::new();
        let task_id = TaskId(42);

        let waker = MinissWaker::create_waker(task_id, &queue as *const VecDeque<_>);

        // Wake the task
        waker.wake();

        // Check that task was added to queue
        assert_eq!(queue.pop_front(), Some(task_id));
    }

    #[test]
    fn test_waker_clone() {
        let mut queue = VecDeque::new();
        let task_id = TaskId(99);

        let waker1 = MinissWaker::create_waker(task_id, &queue as *const VecDeque<_>);
        let waker2 = waker1.clone();

        // Both wakers should work
        waker1.wake();
        waker2.wake();

        // Should have two entries in queue
        assert_eq!(queue.pop_front(), Some(task_id));
        assert_eq!(queue.pop_front(), Some(task_id));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_waker_wake_by_ref() {
        let mut queue = VecDeque::new();
        let task_id = TaskId(123);

        let waker = MinissWaker::create_waker(task_id, &queue as *const VecDeque<_>);

        // Wake by reference
        waker.wake_by_ref();

        // Task should be in queue
        assert_eq!(queue.pop_front(), Some(task_id));

        // Waker should still be usable
        waker.wake();
        assert_eq!(queue.pop_front(), Some(task_id));
    }

    /// Verify that Rc refcount is managed correctly — no leaks, no double-frees.
    #[test]
    fn test_waker_drop_after_clone() {
        let mut queue = VecDeque::new();
        let task_id = TaskId(77);

        let waker = MinissWaker::create_waker(task_id, &queue as *const VecDeque<_>);
        let cloned = waker.clone();

        // Drop the original
        drop(waker);

        // The clone should still work (wake_by_ref doesn't consume)
        cloned.wake_by_ref();
        assert_eq!(queue.pop_front(), Some(task_id));

        // Drop the clone (both should drop cleanly)
        drop(cloned);
    }
}
