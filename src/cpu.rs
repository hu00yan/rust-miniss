use std::cell::UnsafeCell;
use std::ptr;
use std::task::Waker;
use std::time::Instant;

use crate::io::{CompletionKind, IoError, IoProvider, IoToken};
use crate::timer::{TimerId, TimerWheel};

// --- I/O State Management ---

type IoBackend = dyn IoProvider<Completion = (IoToken, Result<CompletionKind, IoError>)>;

#[cfg(miri)]
struct MiriProvider;

#[cfg(miri)]
impl IoProvider for MiriProvider {
    type Completion = (IoToken, Result<CompletionKind, IoError>);

    fn submit(&self, _op: crate::io::Op) -> Result<IoToken, IoError> {
        Err(IoError::Other("I/O backend unavailable under Miri".into()))
    }

    fn drain_completions(&self) -> std::task::Poll<Vec<Self::Completion>> {
        std::task::Poll::Pending
    }
}

pub(crate) fn create_default_io_backend() -> Box<IoBackend> {
    #[cfg(miri)]
    {
        return Box::new(MiriProvider);
    }

    #[cfg(all(not(miri), target_os = "linux", io_backend = "io_uring"))]
    {
        Box::new(
            crate::io::uring::UringBackend::new(4096)
                .unwrap_or_else(|e| panic!("Failed to create io_uring backend: {e}")),
        )
    }

    #[cfg(all(target_os = "macos", io_backend = "kqueue"))]
    {
        Box::new(
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
        Box::new(
            crate::io::epoll::EpollBackend::new()
                .unwrap_or_else(|e| panic!("Failed to create epoll backend: {e}")),
        )
    }

    #[cfg(not(any(
        all(target_os = "linux", io_backend = "io_uring"),
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
/// I/O tokens are sequential slab indices, so we use a flat `Vec` of slots
/// instead of HashMap for O(1) direct-indexed access with zero hashing overhead.
/// The vec grows lazily as needed; resizes are amortized O(1).
///
/// ```compile_fail
/// fn requires_send<T: Send>(_: T) {}
/// requires_send(rust_miniss::cpu::CpuIoState::default());
/// ```
#[repr(C)]
pub struct CpuIoState {
    /// The I/O backend for submitting operations.
    pub io_backend: Box<IoBackend>,
    /// Per-token state packed into a single slot to avoid three separate table lookups.
    /// SAFETY: Single-threaded access only.
    pub io_slots: UnsafeCell<Vec<IoSlot>>,
    /// Task identities completed on this owner thread.
    pub(crate) owner_completions: UnsafeCell<Vec<OwnerCompletion>>,
    pub(crate) owner_completion: UnsafeCell<Option<(IoToken, Result<CompletionKind, IoError>)>>,
    _local: std::marker::PhantomData<std::rc::Rc<()>>,
}

pub(crate) struct OwnerCompletion {
    pub(crate) task_id: crate::waker::TaskId,
    pub(crate) token: IoToken,
    pub(crate) result: Result<CompletionKind, IoError>,
}

#[derive(Default)]
pub struct IoSlot {
    token_id: u64,
    waker: Option<Waker>,
    completed: Option<Result<CompletionKind, IoError>>,
    pending_task: Option<crate::waker::TaskId>,
    discarded: bool,
}

impl Default for CpuIoState {
    fn default() -> Self {
        let mut io_slots = Vec::with_capacity(4096);
        io_slots.resize_with(4096, IoSlot::default);
        Self {
            io_backend: create_default_io_backend(),
            // Pre-fill to the default ring size so the common path can write
            // slots directly without growing the vector first.
            io_slots: UnsafeCell::new(io_slots),
            owner_completions: UnsafeCell::new(Vec::with_capacity(256)),
            owner_completion: UnsafeCell::new(None),
            _local: std::marker::PhantomData,
        }
    }
}

impl std::fmt::Debug for CpuIoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpuIoState")
            .field("io_backend", &"<IoBackend>")
            .field("io_slots", &"<UnsafeCell>")
            .field("owner_completions", &"<UnsafeCell>")
            .finish()
    }
}

impl CpuIoState {
    /// # Safety
    ///
    /// The caller must run on the owning shard and provide a live token. No
    /// other thread may access the owner completion queue concurrently.
    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) unsafe fn push_owner_completion(
        &self,
        task_id: crate::waker::TaskId,
        token: IoToken,
        result: Result<CompletionKind, IoError>,
    ) {
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { &mut *self.owner_completions.get() }.push(OwnerCompletion {
            task_id,
            token,
            result,
        });
    }

    /// # Safety
    ///
    /// The caller must run on the owning shard and provide exclusive access to
    /// the owner completion batch and destination vector.
    #[inline]
    /// Swap the owner completion batch with a caller-owned buffer. Both
    /// vectors retain their allocations, so draining a busy shard does not
    /// allocate a replacement vector or move each completion individually.
    pub(crate) unsafe fn swap_owner_completions(&self, batch: &mut Vec<OwnerCompletion>) {
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        std::mem::swap(unsafe { &mut *self.owner_completions.get() }, batch);
    }

    /// # Safety
    ///
    /// The caller must run on the owning shard; the token must belong to this
    /// runtime and no completion may be published concurrently.
    pub(crate) unsafe fn set_owner_completion(
        &self,
        token: IoToken,
        result: Result<CompletionKind, IoError>,
    ) {
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        *unsafe { &mut *self.owner_completion.get() } = Some((token, result));
    }

    /// # Safety
    ///
    /// The caller must run on the owning shard and pass the token currently
    /// stored in the owner completion slot.
    pub(crate) unsafe fn take_owner_completion(
        &self,
        token: IoToken,
    ) -> Option<Result<CompletionKind, IoError>> {
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let completion = unsafe { &mut *self.owner_completion.get() };
        if completion
            .as_ref()
            .is_some_and(|(current, _)| *current == token)
        {
            Some(completion.take().expect("owner completion was present").1)
        } else {
            None
        }
    }

    /// # Safety
    ///
    /// The caller must run on the owning shard while no completion consumer is
    /// active.
    pub(crate) unsafe fn clear_owner_completion(&self) {
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { &mut *self.owner_completion.get() }.take();
    }

    /// Ensure vec is large enough for the given index (grows by 64-slot chunks).
    ///
    /// # Safety
    ///
    /// The caller must have exclusive access to the shard-local vector through
    /// the owning CPU thread.
    #[inline(always)]
    unsafe fn ensure_capacity<T: Default>(vec: &mut Vec<T>, idx: usize) {
        if idx >= vec.len() {
            // Round up to next multiple of 64 for amortized O(1) growth
            let new_len = (idx + 64) & !(64 - 1);
            vec.resize_with(new_len, T::default);
        }
    }

    /// # Safety
    ///
    /// Callers must be running on the owning CPU thread with exclusive
    /// shard-local access to `slots`.
    #[inline(always)]
    unsafe fn prepare_slot(slots: &mut Vec<IoSlot>, token: IoToken) -> &mut IoSlot {
        let idx = token.slot_index();
        Self::ensure_capacity(slots, idx);
        // SAFETY: ensure_capacity guarantees idx is in-bounds.
        let slot = unsafe { slots.get_unchecked_mut(idx) };
        if slot.token_id != token.id() {
            slot.token_id = token.id();
            slot.waker = None;
            slot.completed = None;
            slot.pending_task = None;
            slot.discarded = false;
        }
        slot
    }

    /// # Safety
    ///
    /// Callers must be running on the owning CPU thread with exclusive
    /// shard-local access to `slots`.
    #[inline(always)]
    unsafe fn live_slot(slots: &mut [IoSlot], token: IoToken) -> Option<&mut IoSlot> {
        let idx = token.slot_index();
        let slot = slots.get_mut(idx)?;
        if slot.token_id == token.id() {
            Some(slot)
        } else {
            None
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
    pub unsafe fn store_waker(&self, token: IoToken, waker: &Waker) {
        let slots = &mut *self.io_slots.get();
        // SAFETY: This method is only called by the owning CPU thread.
        let slot = unsafe { Self::prepare_slot(slots, token) };
        match &mut slot.waker {
            Some(existing) if existing.will_wake(waker) => {}
            slot => *slot = Some(waker.clone()),
        }
    }

    /// Store the waker and pending task for a token with one slot lookup.
    ///
    /// # Safety
    ///
    /// `token` must refer to a live operation owned by this `CpuIoState`, and
    /// callers must ensure slot reuse cannot race with this store.
    #[inline(always)]
    pub unsafe fn store_waker_and_pending_task(
        &self,
        token: IoToken,
        waker: &Waker,
        task_id: Option<crate::waker::TaskId>,
    ) {
        let slots = &mut *self.io_slots.get();
        // SAFETY: This method is only called by the owning CPU thread.
        let slot = unsafe { Self::prepare_slot(slots, token) };
        match &mut slot.waker {
            Some(existing) if existing.will_wake(waker) => {}
            entry => *entry = Some(waker.clone()),
        }
        if let Some(task_id) = task_id {
            if slot.pending_task != Some(task_id) {
                slot.pending_task = Some(task_id);
            }
        }
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
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { Self::live_slot(slots, token) }?.waker.take()
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
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let slot = unsafe { Self::prepare_slot(slots, token) };
        slot.completed = Some(result);
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
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { Self::live_slot(slots, token) }?.completed.take()
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
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { Self::live_slot(slots, token) }
            .and_then(|slot| slot.completed.as_ref())
            .is_some()
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
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let slot = unsafe { Self::prepare_slot(slots, token) };
        if slot.pending_task != Some(task_id) {
            slot.pending_task = Some(task_id);
        }
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
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { Self::live_slot(slots, token) }?
            .pending_task
            .take()
    }

    /// Take the pending task and store the completion result with one slot lookup.
    ///
    /// # Safety
    ///
    /// Must be called from the owning CPU thread. The token must refer to a
    /// live slot owned by this `CpuIoState`.
    #[inline(always)]
    pub unsafe fn take_pending_task_and_store_completed(
        &self,
        token: IoToken,
        result: Result<CompletionKind, IoError>,
    ) -> Option<crate::waker::TaskId> {
        let slots = &mut *self.io_slots.get();
        // SAFETY: This method is only called by the owning CPU thread.
        let slot = unsafe { Self::live_slot(slots, token) }?;
        let task_id = slot.pending_task.take()?;
        slot.completed = Some(result);
        Some(task_id)
    }

    /// Take both the pending task and its waker with one slot lookup.
    ///
    /// # Safety
    ///
    /// Must be called from the owning CPU thread. The token must refer to a
    /// live slot owned by this `CpuIoState`.
    #[inline(always)]
    pub unsafe fn take_pending_task_and_waker(
        &self,
        token: IoToken,
    ) -> Option<(crate::waker::TaskId, Waker)> {
        let slots = &mut *self.io_slots.get();
        // SAFETY: This method is only called by the owning CPU thread.
        let slot = unsafe { Self::live_slot(slots, token) }?;
        Some((slot.pending_task.take()?, slot.waker.take()?))
    }

    /// Store a completion result and take the current waker in one slot lookup.
    ///
    /// # Safety
    ///
    /// Must be called from the owning CPU thread. The token must refer to a
    /// live slot owned by this `CpuIoState`.
    #[inline(always)]
    pub unsafe fn insert_completed_and_take_waker(
        &self,
        token: IoToken,
        result: Result<CompletionKind, IoError>,
    ) -> Option<Waker> {
        let slots = &mut *self.io_slots.get();
        // SAFETY: This method is only called by the owning CPU thread.
        let slot = unsafe { Self::prepare_slot(slots, token) };
        slot.completed = Some(result);
        slot.waker.take()
    }

    /// Mark an operation as abandoned while retaining its slot until the
    /// backend publishes terminal completion. The completion handler then
    /// drops the result and clears the slot instead of resurrecting it.
    /// # Safety
    ///
    /// The caller must own the shard and keep the token slot reserved until
    /// the backend reports terminal completion.
    pub(crate) unsafe fn cancel_and_discard(&self, token: IoToken) {
        let _cancel_requested = self.io_backend.cancel(token);
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let Some(slot) = (unsafe { Self::live_slot(slots, token) }) else {
            return;
        };
        slot.waker = None;
        slot.pending_task = None;
        slot.completed = None;
        // A failed enqueue does not prove that the kernel operation is gone;
        // retain the discard marker until a terminal CQE arrives.
        slot.discarded = true;
    }

    /// Consume a terminal completion for an abandoned operation.
    #[allow(dead_code)]
    /// # Safety
    ///
    /// The caller must own the shard and call this only after terminal
    /// completion for the token has been observed.
    pub(crate) unsafe fn take_discarded(&self, token: IoToken) -> bool {
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let Some(slot) = (unsafe { Self::live_slot(slots, token) }) else {
            return false;
        };
        if !slot.discarded {
            return false;
        }
        slot.token_id = 0;
        slot.discarded = false;
        true
    }

    /// Clear the per-token slot in one lookup.
    ///
    /// # Safety
    ///
    /// Must be called from the owning CPU thread. The token must refer to a
    /// live slot owned by this `CpuIoState`.
    #[inline(always)]
    pub unsafe fn clear_slot(&self, token: IoToken) {
        let slots = &mut *self.io_slots.get();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let Some(slot) = (unsafe { Self::live_slot(slots, token) }) else {
            return;
        };
        slot.token_id = 0;
        slot.waker = None;
        slot.completed = None;
        slot.pending_task = None;
        slot.discarded = false;
    }
}

impl Drop for CpuIoState {
    fn drop(&mut self) {
        // SAFETY: Drop has exclusive access to CpuIoState; no worker can access
        // these UnsafeCell-backed vectors after the owner is being dropped.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe {
            // Vec::clear is O(n) but fast — just sets len to 0,
            // dropping all remaining values.
            (*self.io_slots.get()).clear();
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
    /// The shard currently installed on this thread, if any.
    static CURRENT_CORE_ID: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Scoped installation for all runtime TLS state.
///
/// The guard restores the complete previous context on normal return, panic,
/// and nesting. It deliberately owns no reference to the installed state: the
/// raw pointers are used only while the caller keeps the owner alive.
pub(crate) struct RuntimeContextGuard {
    previous_io: *const CpuIoState,
    previous_timer: *mut TimerWheel,
    previous_task: Option<crate::waker::TaskId>,
    previous_core: Option<usize>,
}

impl RuntimeContextGuard {
    /// # Safety
    /// `io_state` and `timer_wheel` must outlive the returned guard. Callers
    /// must keep both owners on this thread until the guard is dropped.
    pub(crate) unsafe fn enter(
        io_state: &CpuIoState,
        timer_wheel: &mut TimerWheel,
        core_id: Option<usize>,
    ) -> Self {
        let previous_io = CURRENT_IO_STATE.with(|cell| cell.replace(io_state));
        let previous_timer = CURRENT_TIMER_WHEEL.with(|cell| cell.replace(timer_wheel));
        let previous_task = CURRENT_TASK_ID.with(|cell| cell.replace(None));
        let previous_core = CURRENT_CORE_ID.with(|cell| cell.replace(core_id));
        Self {
            previous_io,
            previous_timer,
            previous_task,
            previous_core,
        }
    }
}

impl Drop for RuntimeContextGuard {
    fn drop(&mut self) {
        CURRENT_CORE_ID.with(|cell| cell.set(self.previous_core));
        CURRENT_TASK_ID.with(|cell| cell.set(self.previous_task));
        CURRENT_TIMER_WHEEL.with(|cell| cell.set(self.previous_timer));
        CURRENT_IO_STATE.with(|cell| cell.set(self.previous_io));
    }
}

/// Executes `f` with the current shard I/O state.
///
/// The higher-ranked closure prevents safe code from returning a reference to
/// stack-owned runtime state.
///
/// ```compile_fail
/// let escaped = rust_miniss::cpu::with_io_state(|state| state);
/// let _ = escaped.io_backend.pending_submission_count();
/// ```
#[inline(always)]
pub fn with_io_state<R>(f: impl for<'a> FnOnce(&'a CpuIoState) -> R) -> R {
    CURRENT_IO_STATE.with(|cell| {
        let ptr = cell.get();
        assert!(
            !ptr.is_null(),
            "I/O operation attempted outside of a miniss runtime thread"
        );
        // SAFETY: RuntimeContextGuard installs this pointer only while its
        // owner is alive, and the borrow cannot escape `f`.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        f(unsafe { &*ptr })
    })
}

/// Executes `f` with the current shard I/O state when one is installed.
#[inline(always)]
pub fn try_with_io_state<R>(f: impl for<'a> FnOnce(&'a CpuIoState) -> R) -> Option<R> {
    CURRENT_IO_STATE.with(|cell| {
        let ptr = cell.get();
        if ptr.is_null() {
            None
        } else {
            // SAFETY: see with_io_state.
            Some(f(unsafe { &*ptr }))
        }
    })
}

/// Returns the installed state for unsafe internals that already prove their
/// execution is confined to the owning shard turn.
///
/// # Safety
/// The caller must not retain the reference past the active
/// `RuntimeContextGuard`, nor access it from another thread.
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
pub(crate) unsafe fn current_io_state_unchecked() -> &'static CpuIoState {
    CURRENT_IO_STATE.with(|cell| {
        let ptr = cell.get();
        assert!(
            !ptr.is_null(),
            "I/O operation attempted outside miniss runtime"
        );
        // SAFETY: upheld by the caller's contract above.
        unsafe { &*ptr }
    })
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
pub(crate) fn set_current_task_id(id: Option<crate::waker::TaskId>) {
    CURRENT_TASK_ID.with(|cell| cell.set(id));
}

pub(crate) fn current_core_id() -> Option<usize> {
    CURRENT_CORE_ID.with(std::cell::Cell::get)
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
            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
            Some(f(unsafe { &mut *ptr }))
        }
    })
}

pub(crate) fn register_timer_waker(at: Instant, waker: &Waker) -> Option<TimerId> {
    with_current_timer(|wheel| wheel.schedule(at, waker))
}

pub(crate) fn cancel_timer_waker(id: TimerId) -> bool {
    with_current_timer(|wheel| wheel.cancel(id)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    static_assertions::assert_not_impl_any!(CpuIoState: Send, Sync);
    use crate::io::{CompletionKind, IoError, IoProvider, Op};
    use std::task::Poll;

    struct DummyProvider;

    impl IoProvider for DummyProvider {
        type Completion = (IoToken, Result<CompletionKind, IoError>);

        fn submit(&self, _op: Op) -> Result<IoToken, IoError> {
            Ok(IoToken::from_id(1))
        }

        fn drain_completions(&self) -> Poll<Vec<Self::Completion>> {
            Poll::Pending
        }
    }

    #[test]
    fn cpu_io_slots_do_not_alias_different_token_generations() {
        let state = CpuIoState {
            io_backend: Box::new(DummyProvider),
            io_slots: Default::default(),
            owner_completions: Default::default(),
            owner_completion: Default::default(),
            _local: std::marker::PhantomData,
        };
        let old = IoToken::from_id((1u64 << 32) | 7);
        let new = IoToken::from_id((2u64 << 32) | 7);

        // SAFETY: The test owns this CpuIoState and never shares it across
        // threads, matching the runtime's shard-local access contract.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe {
            state.insert_completed(old, Ok(CompletionKind::Nop));
            assert!(state.has_completed(old));
            assert!(!state.has_completed(new));

            state.insert_completed(new, Ok(CompletionKind::Nop));
            assert!(!state.has_completed(old));
            assert!(state.has_completed(new));

            state.clear_slot(old);
            assert!(state.has_completed(new));
            assert!(state.take_completed(new).is_some());
        }
    }

    #[test]
    fn owner_completion_is_delivered_once_and_cleared_for_stale_tasks() {
        let state = CpuIoState::default();
        let task = crate::waker::TaskId::from_key(crate::waker::TaskKey::new(1, 2, 3, 4));
        let token = IoToken::from_id(19);

        // SAFETY: this test keeps the shard-local state on one thread.
        unsafe {
            state.push_owner_completion(task, token, Ok(CompletionKind::Nop));
            let mut queued = Vec::new();
            state.swap_owner_completions(&mut queued);
            assert_eq!(queued.len(), 1);
            let completion = queued.pop().expect("queued owner completion");
            assert_eq!(completion.task_id, task);
            state.set_owner_completion(completion.token, completion.result);
            assert!(matches!(
                state.take_owner_completion(token),
                Some(Ok(CompletionKind::Nop))
            ));
            assert!(state.take_owner_completion(token).is_none());

            state.set_owner_completion(token, Ok(CompletionKind::Nop));
            state.clear_owner_completion();
            assert!(state.take_owner_completion(token).is_none());
        }
    }

    #[test]
    fn abandoned_slot_discards_late_completion_even_when_cancel_request_is_rejected() {
        let state = CpuIoState::default();
        let token = IoToken::from_id(23);
        // SAFETY: the test owns this shard-local state.
        unsafe {
            let waker = futures::task::noop_waker();
            state.store_waker(token, &waker);
            state.cancel_and_discard(token);
            assert!(state.take_discarded(token));
            assert!(!state.has_completed(token));
        }
    }

    #[test]
    fn runtime_context_guard_restores_timer_task_and_core_on_nesting() {
        let outer_io = CpuIoState::default();
        let mut outer_timer = TimerWheel::default();
        // SAFETY: both owners live until their matching guards are dropped.
        let outer = unsafe { RuntimeContextGuard::enter(&outer_io, &mut outer_timer, Some(7)) };
        set_current_task_id(Some(crate::waker::TaskId(11)));
        let outer_ptr = with_io_state(|state| state as *const _);
        let waker = futures::task::noop_waker();
        let timer = register_timer_waker(Instant::now(), &waker).expect("outer timer");

        let inner_io = CpuIoState::default();
        let mut inner_timer = TimerWheel::default();
        // SAFETY: both owners live until their matching guards are dropped.
        let inner = unsafe { RuntimeContextGuard::enter(&inner_io, &mut inner_timer, Some(8)) };
        set_current_task_id(Some(crate::waker::TaskId(12)));
        assert_eq!(current_core_id(), Some(8));
        assert_eq!(current_task_id(), Some(crate::waker::TaskId(12)));
        assert_ne!(outer_ptr, with_io_state(|state| state as *const _));

        drop(inner);
        assert_eq!(current_core_id(), Some(7));
        assert_eq!(current_task_id(), Some(crate::waker::TaskId(11)));
        assert_eq!(outer_ptr, with_io_state(|state| state as *const _));
        assert!(cancel_timer_waker(timer));

        drop(outer);
        assert!(try_with_io_state(|_| ()).is_none());
        assert_eq!(current_core_id(), None);
        assert_eq!(current_task_id(), None);
    }
}
