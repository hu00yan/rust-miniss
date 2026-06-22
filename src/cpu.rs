use std::cell::RefCell;
use std::collections::HashMap;
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
            crate::io::uring::UringBackend::new(32)
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
#[repr(align(64))] // Cache line alignment to avoid false sharing
pub struct CpuIoState {
    /// The I/O backend for submitting operations.
    pub io_backend: Arc<IoBackend>,
    /// Padding to avoid false sharing
    _pad1: [u8; 56],
    /// Wakers for tasks that are waiting for an I/O operation to complete.
    pub io_wakers: std::sync::Mutex<HashMap<IoToken, Waker>>,
    /// Padding to avoid false sharing
    _pad2: [u8; 56],
    /// Results of completed I/O operations.
    pub completed_io: std::sync::Mutex<HashMap<IoToken, Result<CompletionKind, IoError>>>,
}

impl Default for CpuIoState {
    fn default() -> Self {
        let map = HashMap::new();
        Self {
            io_backend: create_default_io_backend(),
            _pad1: [0u8; 56],
            io_wakers: std::sync::Mutex::new(HashMap::new()),
            _pad2: [0u8; 56],
            completed_io: std::sync::Mutex::new(map),
        }
    }
}

impl std::fmt::Debug for CpuIoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpuIoState")
            .field("io_backend", &"<IoBackend>")
            .field(
                "io_wakers",
                &format!(
                    "HashMap<IoToken, Waker>[{}]",
                    self.io_wakers.lock().unwrap().len()
                ),
            )
            .field("completed_io", &"<Mutex>")
            .finish()
    }
}

impl CpuIoState {
    /// Stores a waker in the slab at the given token's slab index.
    pub fn store_waker(&self, token: IoToken, waker: Waker) {
        self.io_wakers.lock().unwrap().insert(token, waker);
    }

    /// Takes a waker from the slab at the given token's slab index.
    pub fn take_waker(&self, token: IoToken) -> Option<Waker> {
        self.io_wakers.lock().unwrap().remove(&token)
    }
}

impl Drop for CpuIoState {
    fn drop(&mut self) {
        // Clean up any remaining completions and wakers
        self.completed_io.lock().unwrap().clear();
        self.io_wakers.lock().unwrap().clear();
    }
}

thread_local! {
    /// A thread-local reference to the current CPU's `IoState`.
    /// This allows an `IoFuture` to get access to the I/O context of the `Cpu` it's running on.
    pub static CURRENT_CPU_IO_STATE: RefCell<Option<Arc<CpuIoState>>> = const { RefCell::new(None) };
    static CURRENT_TIMER_WHEEL: RefCell<Option<*mut TimerWheel>> = const { RefCell::new(None) };
}

/// Provides access to the I/O state of the current CPU thread.
///
/// Panics if not called from within a `miniss` runtime thread.
pub fn io_state() -> Arc<CpuIoState> {
    CURRENT_CPU_IO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .expect("I/O operation attempted outside of a miniss runtime thread")
            .clone()
    })
}

/// Try to get the I/O state of the current CPU thread.
///
/// Returns `Err` if not called from within a `miniss` runtime thread.
pub fn try_io_state() -> Option<Arc<CpuIoState>> {
    CURRENT_CPU_IO_STATE.with(|cell| cell.borrow().as_ref().cloned())
}

/// Sets the current CPU I/O state for the current thread.
/// This is used to initialize the runtime context for single-threaded execution.
pub fn set_current_io_state(io_state: Arc<CpuIoState>) {
    CURRENT_CPU_IO_STATE.with(|cell| {
        *cell.borrow_mut() = Some(io_state);
    });
}

/// Clears the current CPU I/O state for the current thread.
/// This is used to clean up the runtime context.
pub fn clear_current_io_state() {
    CURRENT_CPU_IO_STATE.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

fn with_current_timer<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut TimerWheel) -> R,
{
    CURRENT_TIMER_WHEEL.with(|cell| {
        (*cell.borrow()).map(|ptr| {
            // SAFETY: The pointer is only set while the owning runtime thread is active
            // and no other mutable references exist. Access is confined to that thread.
            unsafe { f(&mut *ptr) }
        })
    })
}

pub(crate) fn set_current_timer(timer: &mut TimerWheel) {
    CURRENT_TIMER_WHEEL.with(|cell| {
        *cell.borrow_mut() = Some(timer as *mut _);
    });
}

pub(crate) fn clear_current_timer() {
    CURRENT_TIMER_WHEEL.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

pub(crate) fn register_timer_waker(at: Instant, waker: Waker) -> Option<TimerId> {
    with_current_timer(|wheel| wheel.schedule(at, waker))
}

pub(crate) fn cancel_timer_waker(id: TimerId) -> bool {
    with_current_timer(|wheel| wheel.cancel(id)).unwrap_or(false)
}
