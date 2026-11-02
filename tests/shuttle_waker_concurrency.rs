//! Shuttle Concurrency Tests for Seam 1: Waker & Slot Generational Safety.
//!
//! This suite verifies under randomized Shuttle execution interleavings:
//! 1. Concurrent cross-thread wakers coalesce without corrupting ready queues.
//! 2. Stale cross-thread wakes cannot trigger ABA aliasing or corrupt replacement tasks.
//! 3. Multiple threads concurrently acquiring and releasing slots from a shared
//!    generational structure never double-free.
//! 4. Generation exhaustion permanently retires slots instead of wrapping (ABA prevention).

use crossbeam_queue::ArrayQueue;
use rust_miniss::waker::{MinissWaker, ReadyQueue, TaskArena, TaskId, TaskKey};
use shuttle::sync::Mutex;
use std::sync::Arc;

/// A thread-safe generational slot structure that models concurrent slot acquisition
/// and release, enforcing ABA generation monotonicity and double-free rejection.
struct Slot<T> {
    generation: u16,
    value: Option<T>,
}

pub struct ConcurrentTaskSlotManager<T> {
    runtime_generation: u16,
    shard: u16,
    capacity: usize,
    state: Mutex<SlotManagerState<T>>, // arch-audit: allow shuttle test synchronization
}

struct SlotManagerState<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u16>,
    retired_count: usize,
    successful_releases: usize,
    double_free_attempts_blocked: usize,
    stale_releases_blocked: usize,
}

impl<T> ConcurrentTaskSlotManager<T> {
    pub fn new(runtime_generation: u16, shard: u16, capacity: usize) -> Self {
        Self::with_initial_generation(runtime_generation, shard, capacity, 0)
    }

    pub fn with_initial_generation(
        runtime_generation: u16,
        shard: u16,
        capacity: usize,
        initial_generation: u16,
    ) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        let mut free = Vec::with_capacity(capacity);
        for slot in 0..capacity {
            slots.push(Slot {
                generation: initial_generation,
                value: None,
            });
            free.push(slot as u16);
        }
        Self {
            runtime_generation,
            shard,
            capacity,
            state: Mutex::new(SlotManagerState {
                // arch-audit: allow shuttle test synchronization
                slots,
                free,
                retired_count: 0,
                successful_releases: 0,
                double_free_attempts_blocked: 0,
                stale_releases_blocked: 0,
            }),
        }
    }

    /// Acquire a vacant slot. Advances generation monotonically.
    /// If generation reaches `u16::MAX`, the slot is permanently retired without wrapping.
    pub fn acquire(&self, value: T) -> Option<TaskKey> {
        let mut state = self.state.lock().unwrap();
        while let Some(slot_idx) = state.free.pop() {
            let slot = &mut state.slots[slot_idx as usize];
            // Generation must not wrap: if checked_add(1) is None, retire permanently.
            let Some(next_gen) = slot.generation.checked_add(1) else {
                state.retired_count += 1;
                continue;
            };
            slot.generation = next_gen;
            slot.value = Some(value);
            return Some(TaskKey::new(
                self.runtime_generation,
                self.shard,
                slot_idx,
                next_gen,
            ));
        }
        None
    }

    /// Release an occupied slot.
    /// Returns Some(value) on first release.
    /// Returns None if already freed (double-free attempt) or if the generation is stale (ABA attempt).
    pub fn release(&self, key: TaskKey) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        if key.runtime_generation != self.runtime_generation
            || key.shard != self.shard
            || key.slot as usize >= self.capacity
            || key.generation == 0
        {
            state.stale_releases_blocked += 1;
            return None;
        }

        let slot = &mut state.slots[key.slot as usize];
        if slot.generation != key.generation {
            // Stale key / ABA replay attempt: rejected!
            state.stale_releases_blocked += 1;
            return None;
        }

        if slot.value.is_none() {
            // Already released: double-free attempt rejected!
            state.double_free_attempts_blocked += 1;
            return None;
        }

        let val = slot.value.take();
        state.successful_releases += 1;
        state.free.push(key.slot);
        val
    }

    pub fn stats(&self) -> (usize, usize, usize, usize) {
        let state = self.state.lock().unwrap();
        (
            state.successful_releases,
            state.double_free_attempts_blocked,
            state.stale_releases_blocked,
            state.retired_count,
        )
    }

    pub fn is_free_consistent(&self) -> bool {
        let state = self.state.lock().unwrap();
        if state.free.len() + state.retired_count > self.capacity {
            return false;
        }
        let mut seen = vec![false; self.capacity];
        for &slot_idx in &state.free {
            let idx = slot_idx as usize;
            if idx >= self.capacity || seen[idx] || state.slots[idx].value.is_some() {
                return false;
            }
            seen[idx] = true;
        }
        true
    }
}

/// Test 1: Multiple threads concurrently acquiring and releasing slots from a shared
/// structure. Specifically verifies that under randomized Shuttle schedules:
/// - Concurrent racing releases of the same key result in exactly one success (no double-free).
/// - Blocked double-free attempts do not duplicate free list entries.
#[test]
fn test_shuttle_concurrent_slot_acquisition_no_double_free() {
    shuttle::check_random(
        || {
            const CAPACITY: usize = 3;
            let manager = Arc::new(ConcurrentTaskSlotManager::<usize>::new(1, 0, CAPACITY)); // arch-audit: allow shuttle test synchronization

            let mut handles = Vec::new();
            for thread_id in 0..3 {
                let mgr = manager.clone();
                handles.push(shuttle::thread::spawn(move || {
                    for i in 0..4 {
                        let val = thread_id * 10 + i;
                        if let Some(key) = mgr.acquire(val) {
                            // Two racing threads try to release the exact same key concurrently.
                            let mgr1 = mgr.clone();
                            let mgr2 = mgr.clone();
                            let h1 = shuttle::thread::spawn(move || mgr1.release(key));
                            let h2 = shuttle::thread::spawn(move || mgr2.release(key));

                            let res1 = h1.join().unwrap();
                            let res2 = h2.join().unwrap();

                            // Exactly one release must succeed; the other must be detected as double-free.
                            let successes =
                                usize::from(res1.is_some()) + usize::from(res2.is_some());
                            assert_eq!(
                                successes, 1,
                                "exactly one release must succeed for a given key"
                            );
                        }
                    }
                }));
            }

            for h in handles {
                h.join().unwrap();
            }

            assert!(
                manager.is_free_consistent(),
                "free structure must remain strictly consistent"
            );
            let (successes, double_frees, stale_releases, _) = manager.stats();
            assert!(
                double_frees + stale_releases > 0,
                "concurrent racing releases should trigger double-free or stale ABA detection"
            );
            assert_eq!(
                successes + double_frees + stale_releases,
                successes * 2,
                "every successful acquisition had exactly 2 release attempts, each either succeeded, was blocked as double-free, or was blocked as stale ABA"
            );
        },
        60,
    );
}

/// Test 2: Verifies that when a slot reaches `u16::MAX`, it is permanently retired
/// rather than wrapping to an older generation (ABA prevention), and stale keys cannot
/// alias new occupants.
#[test]
fn test_shuttle_slot_generation_exhaustion_no_wrapping() {
    shuttle::check_pct(
        || {
            const CAPACITY: usize = 2;
            // Initialize near exhaustion boundary: generation starts at u16::MAX - 1.
            let manager = Arc::new(ConcurrentTaskSlotManager::<u8>::with_initial_generation(
                // arch-audit: allow shuttle test synchronization
                2,
                1,
                CAPACITY,
                u16::MAX - 1,
            ));

            let mut handles = Vec::new();
            for thread_id in 0..2 {
                let mgr = manager.clone();
                handles.push(shuttle::thread::spawn(move || {
                    let mut acquired_keys = Vec::new();
                    // Each thread repeatedly tries to acquire and release.
                    for _ in 0..5 {
                        if let Some(key) = mgr.acquire(thread_id as u8) {
                            acquired_keys.push(key);
                            let _ = mgr.release(key);
                        }
                    }
                    // Attempting to release previously released stale keys must fail.
                    for stale in acquired_keys {
                        assert!(
                            mgr.release(stale).is_none(),
                            "stale key release must be rejected"
                        );
                    }
                }));
            }

            for h in handles {
                h.join().unwrap();
            }

            // Both slots started at u16::MAX - 1, so each can advance to u16::MAX once,
            // and then MUST be retired permanently without wrapping to 0 or 1.
            assert!(
                manager.acquire(99).is_none(),
                "exhausted manager must reject new allocations rather than wrapping"
            );
            let (_, _, _, retired) = manager.stats();
            assert_eq!(retired, CAPACITY, "all slots must be retired on exhaustion");
            assert!(manager.is_free_consistent());
        },
        50,
        3,
    );
}

/// Test 3: Simulates cross-thread wakers racing with owner task polling and slot reuse.
/// Tests ABA prevention:
/// - Thread 0 (owner) allocates slot 0 (generation 1) and creates waker 1.
/// - Thread 1 holds waker 1.
/// - Thread 0 polls, completes task 1, returns slot 0 to free list.
/// - Thread 0 re-allocates slot 0 (now generation 2) and creates waker 2.
/// - Thread 1 (with stale waker 1) and Thread 2 (with fresh waker 2) concurrently wake.
/// - Thread 0 verifies that stale wake (generation 1) is rejected and does NOT touch or
///   double-free generation 2.
#[test]
fn test_shuttle_cross_thread_wakers_aba_prevention() {
    shuttle::check_random(
        || {
            let ready_queue: ReadyQueue = Arc::new(ArrayQueue::new(8));
            let mut arena = TaskArena::<String>::with_capacity(10, 0, 1);

            // Shard owner creates first task in slot 0 (gen 1)
            let key1 = arena.reserve().expect("slot 0 available for task 1");
            assert_eq!(key1.slot, 0);
            assert_eq!(key1.generation, 1);
            assert!(arena.insert(key1, "task_1".to_string()));

            let task_id_1 = TaskId::from_key(key1);
            let waker1 = MinissWaker::create_waker(task_id_1, ready_queue.clone());

            // Complete task 1 and reuse slot 0 for task 2 (gen 2)
            assert_eq!(arena.remove(key1), Some("task_1".to_string()));
            assert!(arena.is_empty());

            let key2 = arena.reserve().expect("slot 0 reused for task 2");
            assert_eq!(key2.slot, 0);
            assert_eq!(key2.generation, 2, "generation must advance monotonically");
            assert!(arena.insert(key2, "task_2".to_string()));

            let task_id_2 = TaskId::from_key(key2);
            let waker2 = MinissWaker::create_waker(task_id_2, ready_queue.clone());

            // Thread A holds stale waker 1 (gen 1) and Thread B holds current waker 2 (gen 2)
            let w1 = waker1.clone();
            let h_stale = shuttle::thread::spawn(move || {
                w1.wake();
            });

            let w2 = waker2.clone();
            let h_fresh = shuttle::thread::spawn(move || {
                w2.wake();
            });

            h_stale.join().unwrap();
            h_fresh.join().unwrap();

            // Owner thread drains the ready queue and validates identities against arena
            let mut processed_task2 = false;
            while let Some(popped_id) = ready_queue.pop() {
                let popped_key = popped_id.key();
                if popped_key == key1 {
                    // Stale wake delivered: TaskArena MUST reject it!
                    assert_eq!(
                        arena.get(popped_key),
                        None,
                        "stale key must not match active slot"
                    );
                    assert_eq!(
                        arena.remove(popped_key),
                        None,
                        "stale removal must be rejected (no double free)"
                    );
                } else if popped_key == key2 {
                    assert_eq!(
                        arena.get(popped_key),
                        Some(&"task_2".to_string()),
                        "current key must match live occupant"
                    );
                    assert_eq!(arena.remove(popped_key), Some("task_2".to_string()));
                    processed_task2 = true;
                }
            }

            assert!(processed_task2, "active task 2 must have been processed");
            assert!(arena.is_empty());

            // Double free verification on both keys
            assert_eq!(arena.remove(key1), None);
            assert_eq!(arena.remove(key2), None);
            assert!(arena.is_empty());
        },
        50,
    );
}

/// Test 4: Multiple concurrent threads repeatedly calling `wake` and `wake_by_ref`
/// on shared wakers while the owner thread drains, verifying coalescing,
/// clean queue bounds, and no lost wake notifications.
#[test]
fn test_shuttle_waker_coalescing_and_concurrent_firing() {
    shuttle::check_random(
        || {
            let ready_queue: ReadyQueue = Arc::new(ArrayQueue::new(4));
            let task_id = TaskId::from_key(TaskKey::new(5, 2, 0, 1));
            let waker = MinissWaker::create_waker(task_id, ready_queue.clone());

            let mut wake_threads = Vec::new();
            for _ in 0..3 {
                let w = waker.clone();
                wake_threads.push(shuttle::thread::spawn(move || {
                    w.wake_by_ref();
                    w.wake();
                }));
            }

            for h in wake_threads {
                h.join().unwrap();
            }

            // At least one wake must be in the queue, and capacity must not overflow.
            let mut dequeued = 0;
            while let Some(popped) = ready_queue.pop() {
                assert_eq!(popped, task_id);
                dequeued += 1;
            }

            assert!(
                dequeued >= 1,
                "at least one wake notification must be present in queue"
            );
            assert!(
                dequeued <= 4,
                "bounded ready queue must not exceed capacity"
            );
        },
        50,
    );
}
