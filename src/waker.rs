//! Custom Waker implementation
//!
//! This module provides a custom Waker that integrates with our executor
//! to schedule tasks when they become ready.
//!
//! Wakers own bounded, Arc-backed notifier state. They remain safe when moved
//! to another thread or retained after task cancellation/shutdown.

use crossbeam_queue::ArrayQueue;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, AtomicU16, Ordering},
    Arc,
};
use std::task::Waker;

/// Full generational identity for a task slot.
///
/// `slot` is owned by `shard`; `generation` changes on every reuse.  Keeping
/// all four components in the identity prevents a delayed wake/cancel from
/// addressing a replacement task (ABA).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskKey {
    pub runtime_generation: u16,
    pub shard: u16,
    pub slot: u16,
    pub generation: u16,
}

static NEXT_RUNTIME_GENERATION: AtomicU16 = AtomicU16::new(1);

/// Allocate a non-zero runtime identity for task keys. Wraparound is
/// unavoidable in the compact public key, but zero is reserved as invalid.
pub(crate) fn next_runtime_generation() -> u16 {
    let generation = NEXT_RUNTIME_GENERATION.fetch_add(1, Ordering::Relaxed);
    if generation == 0 {
        1
    } else {
        generation
    }
}

impl TaskKey {
    pub const fn new(runtime_generation: u16, shard: u16, slot: u16, generation: u16) -> Self {
        Self {
            runtime_generation,
            shard,
            slot,
            generation,
        }
    }

    pub const fn is_live(self) -> bool {
        self.generation != 0
    }
}

/// Bounded shard-local arena with generational slot allocation.
///
/// A slot is returned to the free list only through `remove`; `reserve` then
/// advances its generation before handing it out again.  Keys from a prior
/// occupant therefore fail `get`/`remove` instead of touching the replacement.
pub struct TaskArena<T> {
    runtime_generation: u16,
    shard: u16,
    slots: Vec<Option<T>>,
    generations: Vec<u16>,
    free: VecDeque<u16>,
    _local: PhantomData<Rc<()>>,
}

impl<T> TaskArena<T> {
    pub fn with_capacity(runtime_generation: u16, shard: u16, capacity: usize) -> Self {
        assert!(
            capacity <= u16::MAX as usize + 1,
            "task arena capacity exceeds key space"
        );
        let mut free = VecDeque::with_capacity(capacity);
        for slot in 0..capacity {
            free.push_back(slot as u16);
        }
        Self {
            runtime_generation,
            shard,
            slots: (0..capacity).map(|_| None).collect(),
            generations: vec![0; capacity],
            free,
            _local: PhantomData,
        }
    }

    /// Reserve a vacant slot. `None` means bounded capacity is exhausted.
    pub fn reserve(&mut self) -> Option<TaskKey> {
        let slot = self.free.pop_front()?;
        let generation = self.generations[slot as usize].wrapping_add(1).max(1);
        self.generations[slot as usize] = generation;
        Some(TaskKey::new(
            self.runtime_generation,
            self.shard,
            slot,
            generation,
        ))
    }

    pub fn insert(&mut self, key: TaskKey, value: T) -> bool {
        let Some(slot) = self.slots.get_mut(key.slot as usize) else {
            return false;
        };
        if key.runtime_generation != self.runtime_generation
            || key.shard != self.shard
            || key.generation == 0
            || self.generations[key.slot as usize] != key.generation
            || slot.is_some()
        {
            return false;
        }
        *slot = Some(value);
        true
    }

    /// Admit an externally allocated key (for example, a bounded cross-shard
    /// message). The first key initializes a vacant slot; every reuse must
    /// advance the arena generation, so a delayed old message cannot recreate
    /// a terminal occupant.
    pub fn insert_external(&mut self, key: TaskKey, value: T) -> bool {
        if !self.can_insert_external(key) {
            return false;
        }
        let slot_index = key.slot as usize;
        self.generations[slot_index] = key.generation;
        self.free.retain(|free_slot| *free_slot != key.slot);
        self.slots[slot_index] = Some(value);
        true
    }

    /// Check owner-side admission without consuming the task value. This lets
    /// a bounded caller retain an already accepted task while its selected
    /// slot is still occupied, then retry after terminal cleanup.
    pub(crate) fn can_insert_external(&self, key: TaskKey) -> bool {
        if key.runtime_generation != self.runtime_generation
            || key.shard != self.shard
            || key.generation == 0
        {
            return false;
        }
        let slot_index = key.slot as usize;
        if self.slots.get(slot_index).is_none_or(Option::is_some) {
            return false;
        }
        let current_generation = self.generations[slot_index];
        let generation_delta = key.generation.wrapping_sub(current_generation);
        current_generation == 0 || (generation_delta != 0 && generation_delta < 0x8000)
    }

    pub fn get(&self, key: TaskKey) -> Option<&T> {
        if self.matches(key) {
            self.slots[key.slot as usize].as_ref()
        } else {
            None
        }
    }

    pub fn get_mut(&mut self, key: TaskKey) -> Option<&mut T> {
        if self.matches(key) {
            self.slots[key.slot as usize].as_mut()
        } else {
            None
        }
    }

    pub fn remove(&mut self, key: TaskKey) -> Option<T> {
        if !self.matches(key) {
            return None;
        }
        let value = self.slots[key.slot as usize].take();
        if value.is_some() {
            self.free.push_back(key.slot);
        }
        value
    }

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// Iterate over occupied slots without exposing arena internals.
    pub fn iter(&self) -> impl Iterator<Item = Option<&T>> {
        self.slots.iter().map(Option::as_ref)
    }

    /// Drop all occupants and make every slot available for a future
    /// generation. Generation values are intentionally retained.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            slot.take();
        }
        self.free.clear();
        self.free
            .extend((0..self.slots.len()).map(|slot| slot as u16));
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    fn matches(&self, key: TaskKey) -> bool {
        key.runtime_generation == self.runtime_generation
            && key.shard == self.shard
            && key.generation != 0
            && self.generations.get(key.slot as usize) == Some(&key.generation)
            && self.slots[key.slot as usize].is_some()
    }
}

/// A task ID that uniquely identifies a task in the executor.
///
/// Kept as a compact public wrapper for API compatibility; use [`TaskId::key`]
/// whenever addressing task storage. New IDs are always encoded from a full
/// [`TaskKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub u64);

impl TaskId {
    const FIELD_MASK: u64 = 0xffff;

    pub fn from_key(key: TaskKey) -> Self {
        Self(
            (key.runtime_generation as u64) << 48
                | (key.shard as u64) << 32
                | (key.slot as u64) << 16
                | key.generation as u64,
        )
    }

    pub fn key(self) -> TaskKey {
        TaskKey::new(
            (self.0 >> 48) as u16,
            (self.0 >> 32) as u16,
            ((self.0 >> 16) & Self::FIELD_MASK) as u16,
            (self.0 & Self::FIELD_MASK) as u16,
        )
    }

    pub fn cpu_id(&self) -> usize {
        self.key().shard as usize
    }

    pub fn sequence(&self) -> u32 {
        self.key().slot as u32
    }

    pub fn new(core_id: usize, sequence: u32) -> Self {
        Self::new_for_runtime(1, core_id, sequence)
    }

    pub fn new_for_runtime(runtime_generation: u16, core_id: usize, sequence: u32) -> Self {
        assert!(
            core_id <= u16::MAX as usize,
            "core id exceeds task key space"
        );
        // Keep the historical `(core, u32 sequence)` constructor while
        // splitting sequence into bounded slot + generation fields. This
        // prevents the old u32 truncation from silently aliasing a live slot.
        let slot = sequence as u16;
        let generation = (sequence >> 16).saturating_add(1).min(u16::MAX as u32) as u16;
        Self::from_key(TaskKey::new(
            runtime_generation,
            core_id as u16,
            slot,
            generation,
        ))
    }
}

/// Bounded, cross-thread-safe ready queue retained by each waker.
pub type ReadyQueue = Arc<ArrayQueue<TaskId>>;

pub(crate) struct WakerState {
    task_id: TaskId,
    queue: ReadyQueue,
    owner_thread: std::thread::Thread,
    scheduled: AtomicBool,
    /// Set when the bounded queue was full. The owner retries this marker
    /// after draining the queue so a one-shot wake cannot be lost.
    overflowed: AtomicBool,
    active: AtomicBool,
}

impl WakerState {
    pub(crate) fn new(task_id: TaskId, queue: ReadyQueue) -> Arc<Self> {
        Arc::new(Self {
            task_id,
            queue,
            owner_thread: std::thread::current(),
            scheduled: AtomicBool::new(false),
            overflowed: AtomicBool::new(false),
            active: AtomicBool::new(true),
        })
    }
    #[inline]
    pub(crate) fn schedule(&self) {
        if !self.active.load(Ordering::Acquire) || self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.queue.push(self.task_id).is_err() {
            // Keep a sticky marker. There may be no later wake call after a
            // one-shot future observes a full queue; the owner-side retry
            // closes that lost-wake window without growing the queue.
            self.overflowed.store(true, Ordering::Release);
            self.scheduled.store(false, Ordering::Release);
            // Wake the owner even when the queue is full: the owner-side
            // retry path is what makes the sticky marker observable before
            // the worker parks.
            self.owner_thread.unpark();
            return;
        }
        self.overflowed.store(false, Ordering::Release);
        self.owner_thread.unpark();
    }

    /// Retry a previously failed enqueue after the owner made queue capacity
    /// available. Returns whether the retry successfully queued the task.
    pub(crate) fn retry_overflowed(&self) -> bool {
        if !self.overflowed.load(Ordering::Acquire) {
            return false;
        }
        self.schedule();
        !self.overflowed.load(Ordering::Acquire)
    }
    pub(crate) fn begin_poll(&self) {
        self.scheduled.store(false, Ordering::Release);
        self.overflowed.store(false, Ordering::Release);
    }
    pub(crate) fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        self.scheduled.store(false, Ordering::Release);
        self.overflowed.store(false, Ordering::Release);
    }
}

struct ArcWakeImpl(Arc<WakerState>);
impl futures::task::ArcWake for ArcWakeImpl {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.schedule();
    }
}

pub(crate) fn create_waker(task_id: TaskId, queue: ReadyQueue) -> (Waker, Arc<WakerState>) {
    let state = WakerState::new(task_id, queue);
    (
        futures::task::waker(Arc::new(ArcWakeImpl(state.clone()))),
        state,
    )
}

pub struct MinissWaker;

// Arc-backed waker state is Send + Sync by construction.

impl MinissWaker {
    /// Create a new waker for the given task
    ///
    /// Takes an Arc-owned bounded ready queue.
    pub fn create_waker(task_id: TaskId, queue: ReadyQueue) -> Waker {
        create_waker(task_id, queue).0
    }
}

/// Create a dummy waker for simple polling scenarios
/// This is used when we don't need proper task scheduling
pub fn dummy_waker() -> Waker {
    futures::task::noop_waker()
}

#[cfg(test)]
mod tests {
    use super::*;

    static_assertions::assert_not_impl_any!(TaskArena<()>: Send, Sync);
    static_assertions::assert_impl_all!(WakerState: Send, Sync);
    static_assertions::assert_impl_all!(TaskId: Copy, Send, Sync);
    static_assertions::assert_impl_all!(ReadyQueue: Clone, Send, Sync);

    #[test]
    fn task_arena_rejects_stale_key_after_slot_reuse() {
        let mut arena = TaskArena::with_capacity(7, 2, 1);
        let old = arena.reserve().expect("first slot available");
        assert!(arena.insert(old, "old"));
        assert_eq!(arena.get(old), Some(&"old"));
        assert_eq!(arena.remove(old), Some("old"));

        let replacement = arena
            .reserve()
            .expect("slot returned after terminal cleanup");
        assert_eq!(replacement.slot, old.slot);
        assert_ne!(replacement.generation, old.generation);
        assert!(arena.insert(replacement, "replacement"));
        assert_eq!(arena.get(old), None);
        assert_eq!(arena.remove(old), None);
        assert_eq!(arena.get(replacement), Some(&"replacement"));
    }

    #[test]
    fn task_arena_is_bounded_and_rejects_wrong_shard() {
        let mut arena = TaskArena::<u8>::with_capacity(3, 1, 1);
        let key = arena.reserve().unwrap();
        assert!(arena.reserve().is_none());
        assert!(!arena.insert_external(TaskKey::new(3, 1, 1, 1), 9));
        assert!(!arena.insert(TaskKey::new(3, 2, key.slot, key.generation), 1));
        assert!(arena.insert(key, 1));
    }

    #[test]
    fn task_arena_external_admission_advances_generation() {
        let mut arena = TaskArena::<u8>::with_capacity(4, 3, 1);
        let first = TaskKey::new(4, 3, 0, 1);
        assert!(arena.insert_external(first, 7));
        assert!(!arena.insert_external(first, 8));
        assert_eq!(arena.remove(first), Some(7));
        let next = TaskKey::new(4, 3, 0, 2);
        assert!(arena.insert_external(next, 8));
        assert_eq!(arena.get(first), None);
        assert_eq!(arena.get(next), Some(&8));
    }

    #[test]
    fn task_arena_external_admission_can_skip_rejected_generation_without_reviving_it() {
        let mut arena = TaskArena::<u8>::with_capacity(4, 3, 1);
        let first = TaskKey::new(4, 3, 0, 1);
        let skipped = TaskKey::new(4, 3, 0, 2);
        let replacement = TaskKey::new(4, 3, 0, 3);

        assert!(arena.insert_external(first, 7));
        assert!(!arena.insert_external(skipped, 8));
        assert_eq!(arena.remove(first), Some(7));
        assert!(arena.insert_external(replacement, 9));
        assert!(!arena.insert_external(skipped, 8));
        assert_eq!(arena.get(replacement), Some(&9));
    }

    #[test]
    fn task_id_round_trip_preserves_full_identity() {
        let key = TaskKey::new(19, 7, 513, 42);
        let id = TaskId::from_key(key);
        assert_eq!(id.key(), key);
        assert_eq!(id.cpu_id(), 7);
        assert_eq!(id.sequence(), 513);
    }

    #[test]
    fn test_waker_creation() {
        let queue = Arc::new(ArrayQueue::new(16));
        let task_id = TaskId(42);
        let waker = MinissWaker::create_waker(task_id, queue.clone());

        // Wake the task
        waker.wake();

        // Check that task was added to queue
        assert_eq!(queue.pop(), Some(task_id));
    }

    #[test]
    fn test_waker_clone() {
        let queue = Arc::new(ArrayQueue::new(16));
        let task_id = TaskId(99);

        let waker1 = MinissWaker::create_waker(task_id, queue.clone());
        let waker2 = waker1.clone();

        // Both wakers should work
        waker1.wake();
        waker2.wake();

        // Should have two entries in queue
        assert_eq!(queue.pop(), Some(task_id));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn test_waker_wake_by_ref() {
        let queue = Arc::new(ArrayQueue::new(16));
        let task_id = TaskId(123);

        let waker = MinissWaker::create_waker(task_id, queue.clone());

        // Wake by reference
        waker.wake_by_ref();

        // Task should be in queue
        assert_eq!(queue.pop(), Some(task_id));

        // Waker should still be usable
        waker.wake();
        assert_eq!(queue.pop(), None); // deduplicated while still scheduled
    }

    /// Verify cloned Arc-backed wakers drop cleanly.
    #[test]
    fn test_waker_drop_after_clone() {
        let queue = Arc::new(ArrayQueue::new(16));
        let task_id = TaskId(77);

        let waker = MinissWaker::create_waker(task_id, queue.clone());
        let cloned = waker.clone();

        // Drop the original
        drop(waker);

        // The clone should still work (wake_by_ref doesn't consume)
        cloned.wake_by_ref();
        assert_eq!(queue.pop(), Some(task_id));

        // Drop the clone (both should drop cleanly)
        drop(cloned);
    }

    #[test]
    fn escaped_waker_wakes_from_other_thread_and_late_wake_is_harmless() {
        let queue = Arc::new(ArrayQueue::new(16));
        let (waker, state) = create_waker(TaskId(88), queue.clone());
        let thread_waker = waker.clone();
        std::thread::spawn(move || thread_waker.wake())
            .join()
            .unwrap();
        assert_eq!(queue.pop(), Some(TaskId(88)));
        state.deactivate();
        waker.wake_by_ref();
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn full_ready_queue_retries_sticky_wake_after_drain() {
        let queue = Arc::new(ArrayQueue::new(1));
        let occupied = TaskId::new(0, 1);
        let pending = TaskId::new(0, 2);
        queue.push(occupied).unwrap();

        let (_waker, state) = create_waker(pending, queue.clone());
        state.schedule();
        assert_eq!(queue.pop(), Some(occupied));

        // No second wake is issued: owner-side retry must deliver the
        // original one-shot wake once capacity is available.
        assert!(state.retry_overflowed());
        assert_eq!(queue.pop(), Some(pending));
    }
}
