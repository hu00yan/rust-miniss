use std::cell::UnsafeCell;
use std::ptr;
use std::sync::Arc;
use std::task::Waker;
use std::time::Instant;

use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use crate::timer::{TimerId, TimerWheel};

// --- I/O State Management ---

type IoBackend = dyn IoProvider<Completion = (IoToken, Op, Result<CompletionKind, IoError>)>;

pub(crate) fn create_default_io_backend() -> Arc<IoBackend> {
    #[cfg(all(target_os = "linux", io_backend = "io_uring", feature = "io_uring"))]
    {
        Arc::new(
            crate::io::uring::UringBackend::new(4096)
                .unwrap_or_else(|e| panic!("Failed to create io_uring backend: {e}")),
        )
    }

    #[cfg(all(target_os = "macos", io_backend = "kqueue"))]
    {
        Arc::new(
            crate::io::kqueue::KqueueBackend::new()
                .unwrap_or_else(|e| panic!("Failed to create kqueue backend: {e}")),
        )
    }

    #[cfg(all(
        unix,
        any(
            io_backend = "epoll",
            all(not(target_os = "linux"), not(target_os = "macos"))
        )
    ))]
    {
        Arc::new(
            crate::io::epoll::EpollBackend::new()
                .unwrap_or_else(|e| panic!("Failed to create epoll backend: {e}")),
        )
    }

    #[cfg(not(any(
        all(target_os = "linux", io_backend = "io_uring", feature = "io_uring"),
        all(target_os = "macos", io_backend = "kqueue"),
        all(
            unix,
            any(
                io_backend = "epoll",
                all(not(target_os = "linux"), not(target_os = "macos"))
            )
        )
    )))]
    {
        panic!("No I/O backend enabled. Please enable io_uring, epoll, or kqueue support.");
    }
}

/// Holds the state related to pending and completed I/O operations for a CPU.
/// This is shared between the `Cpu` and any `IoFuture`s running on it.
///
/// SAFETY: All fields are accessed only from the owning CPU thread.
/// The UnsafeCell is safe because thread-per-core guarantees single-threaded access.
///
/// I/O tokens are sequential slab indices, so we use `Vec<Option<T>>` instead of
/// HashMap for O(1) direct-indexed access with zero hashing overhead.
/// The vec grows lazily as needed; resizes are amortized O(1).
#[repr(C)]
pub struct CpuIoState {
    /// The I/O backend for submitting operations.
    pub io_backend: Arc<IoBackend>,
    /// Wakers for tasks waiting for I/O completions.
    /// Indexed by IoToken::id() as usize. O(1) no hash.
    /// SAFETY: Single-threaded access only.
    pub io_wakers: UnsafeCell<Vec<Option<Waker>>>,
    /// Results of completed I/O operations.
    /// Indexed by IoToken::id() as usize. O(1) no hash.
    /// SAFETY: Single-threaded access only.
    pub completed_io: UnsafeCell<Vec<Option<Result<CompletionKind, IoError>>>>,
    /// Mapping from IoToken to TaskId for inline completion processing.
    /// Indexed by IoToken::id() as usize. O(1) no hash.
    pub pending_tasks: UnsafeCell<Vec<Option<crate::waker::TaskId>>>,
}

// SAFETY: CpuIoState is only accessed from the owning CPU thread.
unsafe impl Send for CpuIoState {}
unsafe impl Sync for CpuIoState {}

impl Default for CpuIoState {
    fn default() -> Self {
        Self {
            io_backend: create_default_io_backend(),
            // Pre-allocate to full ring size (4096) to eliminate repeated
            // ensure_capacity() → heap-alloc resizes on every new I/O token.
            io_wakers: UnsafeCell::new(Vec::with_capacity(4096)),
            completed_io: UnsafeCell::new(Vec::with_capacity(4096)),
            pending_tasks: UnsafeCell::new(Vec::with_capacity(4096)),
        }
    }
}

impl std::fmt::Debug for CpuIoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpuIoState")
            .field("io_backend", &"<IoBackend>")
            .field("io_wakers", &"<UnsafeCell>")
            .field("completed_io", &"<UnsafeCell>")
            .finish()
    }
}

impl CpuIoState {
    /// Ensure vec is large enough for the given index (grows by 64-slot chunks).
    #[inline(always)]
    unsafe fn ensure_capacity<T>(vec: &mut Vec<Option<T>>, idx: usize) {
        if idx >= vec.len() {
            // Round up to next multiple of 64 for amortized O(1) growth
            let new_len = (idx + 64) & !(64 - 1);
            vec.resize_with(new_len, || None);
        }
    }

    /// Stores a waker in the vec for the given token.
    /// SAFETY: Must be called from the owning CPU thread.
    #[inline(always)]
    /// Stores the waker for a pending I/O token.
    ///
    /// # Safety
    ///
    /// `token` must refer to a live operation owned by this `CpuIoState`, and
    /// callers must ensure slot reuse cannot race with this store.
    pub unsafe fn store_waker(&self, token: IoToken, waker: Waker) {
        let idx = token.id() as usize;
        let wakers = &mut *self.io_wakers.get();
        Self::ensure_capacity(wakers, idx);
        wakers[idx] = Some(waker);
    }

    /// Takes a waker from the vec for the given token.
    /// SAFETY: Must be called from the owning CPU thread.
    #[inline(always)]
    /// Removes and returns the waker for a pending I/O token.
    ///
    /// # Safety
    ///
    /// `token` must refer to a live operation owned by this `CpuIoState`, and
    /// callers must ensure the slot has not been released for reuse.
    pub unsafe fn take_waker(&self, token: IoToken) -> Option<Waker> {
        let idx = token.id() as usize;
        let wakers = &mut *self.io_wakers.get();
        wakers.get_mut(idx)?.take()
    }

    /// Insert a completed IO result.
    /// SAFETY: Must be called from the owning CPU thread.
    #[inline(always)]
    /// Stores the completion result for a token.
    ///
    /// # Safety
    ///
    /// `token` must refer to a live operation owned by this `CpuIoState`, and
    /// only one completion may be stored for a token before it is consumed.
    pub unsafe fn insert_completed(&self, token: IoToken, result: Result<CompletionKind, IoError>) {
        let idx = token.id() as usize;
        let completed = &mut *self.completed_io.get();
        Self::ensure_capacity(completed, idx);
        completed[idx] = Some(result);
    }

    /// Remove and return a completed IO result.
    /// SAFETY: Must be called from the owning CPU thread.
    #[inline(always)]
    /// Removes and returns the completion result for a token.
    ///
    /// # Safety
    ///
    /// `token` must refer to a live operation owned by this `CpuIoState`, and
    /// callers must ensure the token has not been recycled.
    pub unsafe fn take_completed(&self, token: IoToken) -> Option<Result<CompletionKind, IoError>> {
        let idx = token.id() as usize;
        let completed = &mut *self.completed_io.get();
        completed.get_mut(idx)?.take()
    }

    /// Check if a completed IO result exists without removing it.
    /// SAFETY: Must be called from the owning CPU thread.
    #[inline(always)]
    /// Returns whether a token has a stored completion.
    ///
    /// # Safety
    ///
    /// `token` must refer to a live operation owned by this `CpuIoState`, and
    /// callers must ensure the token has not been recycled.
    pub unsafe fn has_completed(&self, token: IoToken) -> bool {
        let idx = token.id() as usize;
        let completed = &*self.completed_io.get();
        completed.get(idx).and_then(|o| o.as_ref()).is_some()
    }

    /// Store the task_id that is waiting for this token.
    /// SAFETY: Must be called from the owning CPU thread.
    #[inline(always)]
    /// Stores the task waiting on a pending I/O token.
    ///
    /// # Safety
    ///
    /// `token` must refer to a live operation owned by this `CpuIoState`, and
    /// the task id must remain valid until the operation completes or is
    /// cancelled.
    pub unsafe fn store_pending_task(&self, token: IoToken, task_id: crate::waker::TaskId) {
        let idx = token.id() as usize;
        let tasks = &mut *self.pending_tasks.get();
        Self::ensure_capacity(tasks, idx);
        tasks[idx] = Some(task_id);
    }

    /// Remove and return the task_id for a token.
    /// SAFETY: Must be called from the owning CPU thread.
    #[inline(always)]
    /// Removes and returns the task waiting on a pending I/O token.
    ///
    /// # Safety
    ///
    /// `token` must refer to a live operation owned by this `CpuIoState`, and
    /// callers must ensure the token has not been recycled.
    pub unsafe fn take_pending_task(&self, token: IoToken) -> Option<crate::waker::TaskId> {
        let idx = token.id() as usize;
        let tasks = &mut *self.pending_tasks.get();
        tasks.get_mut(idx)?.take()
    }
}

impl Drop for CpuIoState {
    fn drop(&mut self) {
        unsafe {
            // Vec::clear is O(n) but fast — just sets len to 0,
            // dropping all remaining values.
            (*self.completed_io.get()).clear();
            (*self.io_wakers.get()).clear();
            (*self.pending_tasks.get()).clear();
        }
    }
}

thread_local! {
    /// Thread-local raw pointer to the current CPU's IoState.
    /// Zero-cost access — no Arc clone, no Mutex, no atomic.
    static CURRENT_IO_STATE: std::cell::Cell<*const CpuIoState> = const { std::cell::Cell::new(ptr::null()) };
    static CURRENT_TIMER_WHEEL: std::cell::Cell<*mut TimerWheel> = const { std::cell::Cell::new(ptr::null_mut()) };
    /// Thread-local current TaskId, set before polling each task.
    /// Used by IoFuture to register for inline completion.
    static CURRENT_TASK_ID: std::cell::Cell<Option<crate::waker::TaskId>> =
        const { std::cell::Cell::new(None) };
}

/// Provides access to the I/O state of the current CPU thread.
///
/// Uses a raw pointer — zero atomic overhead, zero Mutex.
/// Panics if not called from within a `miniss` runtime thread.
#[inline(always)]
pub fn io_state() -> &'static CpuIoState {
    CURRENT_IO_STATE.with(|cell| {
        let ptr = cell.get();
        assert!(
            !ptr.is_null(),
            "I/O operation attempted outside of a miniss runtime thread"
        );
        // SAFETY: The pointer is valid for the lifetime of the CPU thread.
        // The CpuCore owns the CpuIoState and sets the pointer before running tasks.
        unsafe { &*ptr }
    })
}

/// Try to get the I/O state of the current CPU thread.
#[inline(always)]
pub fn try_io_state() -> Option<&'static CpuIoState> {
    CURRENT_IO_STATE.with(|cell| {
        let ptr = cell.get();
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    })
}

/// Sets the current CPU I/O state for the current thread.
pub fn set_current_io_state(io_state: &CpuIoState) {
    CURRENT_IO_STATE.with(|cell| {
        cell.set(io_state as *const CpuIoState);
    });
}

/// Clears the current CPU I/O state for the current thread.
pub fn clear_current_io_state() {
    CURRENT_IO_STATE.with(|cell| {
        cell.set(ptr::null());
    });
}

/// Get the current task ID (set before polling each task).
#[inline(always)]
pub fn current_task_id() -> Option<crate::waker::TaskId> {
    CURRENT_TASK_ID.with(|cell| cell.get())
}

/// Set the current task ID for inline completion registration.
///
/// SAFETY: Does NOT touch CURRENT_IO_STATE. That's set once by the core
/// event loop and must remain valid for all task polling. A previous bug
/// cleared it here, which caused every I/O op to panic with a null pointer.
#[inline(always)]
pub fn set_current_task_id(id: Option<crate::waker::TaskId>) {
    CURRENT_TASK_ID.with(|cell| cell.set(id));
}

fn with_current_timer<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut TimerWheel) -> R,
{
    CURRENT_TIMER_WHEEL.with(|cell| {
        let ptr = cell.get();
        if ptr.is_null() {
            None
        } else {
            // SAFETY: The pointer is only set while the owning runtime thread is active
            // and no other mutable references exist. Access is confined to that thread.
            Some(f(unsafe { &mut *ptr }))
        }
    })
}

pub(crate) fn set_current_timer(timer: &mut TimerWheel) {
    CURRENT_TIMER_WHEEL.with(|cell| {
        cell.set(timer as *mut _);
    });
}

pub(crate) fn clear_current_timer() {
    CURRENT_TIMER_WHEEL.with(|cell| {
        cell.set(ptr::null_mut());
    });
}

pub(crate) fn register_timer_waker(at: Instant, waker: Waker) -> Option<TimerId> {
    with_current_timer(|wheel| wheel.schedule(at, waker))
}

pub(crate) fn cancel_timer_waker(id: TimerId) -> bool {
    with_current_timer(|wheel| wheel.cancel(id)).unwrap_or(false)
}
