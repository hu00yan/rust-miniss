//! Custom Waker implementation
//! 
//! This module provides a custom Waker that integrates with our executor
//! to schedule tasks when they become ready.

use std::sync::Arc;
use std::task::{RawWaker, RawWakerVTable, Waker};
use crossbeam_queue::SegQueue;

/// A task ID that uniquely identifies a task in the executor
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub(crate) u64);

impl TaskId {
    pub fn cpu_id(&self) -> usize {
        (self.0 >> 32) as usize
    }
}

/// Waker implementation that can reschedule tasks
pub struct MinissWaker {
    task_id: TaskId,
    queue: Arc<SegQueue<TaskId>>,
}

impl MinissWaker {
    /// Create a new waker for the given task
    pub fn new(task_id: TaskId, queue: Arc<SegQueue<TaskId>>) -> Waker {
        let waker = Arc::new(MinissWaker { task_id, queue });
        let raw_waker = RawWaker::new(Arc::into_raw(waker) as *const (), &VTABLE);
        unsafe { Waker::from_raw(raw_waker) }
    }

    /// Wake the task by adding it to the run queue
    fn wake_impl(&self) {
        self.queue.push(self.task_id);
    }
}

// Raw waker implementation
static VTABLE: RawWakerVTable = RawWakerVTable::new(
    waker_clone,
    waker_wake,
    waker_wake_by_ref,
    waker_drop,
);

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    let waker = Arc::from_raw(data as *const MinissWaker);
    let cloned = waker.clone();
    std::mem::forget(waker); // Don't drop the original
    RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    let waker = Arc::from_raw(data as *const MinissWaker);
    waker.wake_impl();
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    let waker = &*(data as *const MinissWaker);
    waker.wake_impl();
}

unsafe fn waker_drop(data: *const ()) {
    Arc::from_raw(data as *const MinissWaker);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_waker_creation() {
        let queue = Arc::new(SegQueue::new());
        let task_id = TaskId(42);
        
        let waker = MinissWaker::new(task_id, queue.clone());
        
        // Wake the task
        waker.wake();
        
        // Check that task was added to queue
        assert_eq!(queue.pop(), Some(task_id));
    }

    #[test]
    fn test_waker_clone() {
        let queue = Arc::new(SegQueue::new());
        let task_id = TaskId(99);
        
        let waker1 = MinissWaker::new(task_id, queue.clone());
        let waker2 = waker1.clone();
        
        // Both wakers should work
        waker1.wake();
        waker2.wake();
        
        // Should have two entries in queue
        assert_eq!(queue.pop(), Some(task_id));
        assert_eq!(queue.pop(), Some(task_id));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn test_waker_wake_by_ref() {
        let queue = Arc::new(SegQueue::new());
        let task_id = TaskId(123);
        
        let waker = MinissWaker::new(task_id, queue.clone());
        
        // Wake by reference
        waker.wake_by_ref();
        
        // Task should be in queue
        assert_eq!(queue.pop(), Some(task_id));
        
        // Waker should still be usable
        waker.wake();
        assert_eq!(queue.pop(), Some(task_id));
    }
}
