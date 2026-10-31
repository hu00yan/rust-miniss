//! Integration-test shutdown-quiescence oracle (`assert_quiescent!`).
//!
//! Mirror of the `#[cfg(test)]` `QuiescenceGuard` in `src/multicore.rs`
//! (`mod tests`) — integration tests cannot see lib `#[cfg(test)]` items, so
//! the small helper is duplicated here. Keep both copies under 60 code lines
//! and cross-referenced.
//!
//! fd tolerance (+2): the fd reader itself plus one-time lazy machinery
//! (tracing/metrics control plane) first touched inside the measured window.
//! Deliberately far below any real leak signal — the guarded tests hold
//! several sockets or descriptor leases, so a per-operation leak of the class
//! this guard targets cannot hide under the bar. An unreadable fd directory
//! degrades the guard to snapshot-only evidence.

use rust_miniss::MultiCoreRuntime;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FD_TOLERANCE: usize = 2;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const SETTLE_WINDOW: Duration = Duration::from_secs(2);

fn open_fd_count() -> usize {
    #[cfg(target_os = "linux")]
    let dir = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    let dir = "/dev/fd";
    std::fs::read_dir(dir)
        .map(|entries| entries.count())
        .unwrap_or(0)
}

/// Own the runtime under test; `finish_quiescent` applies both oracle halves.
/// Drop bounds cleanup so an assertion panic cannot hang the suite.
pub struct QuiescenceGuard {
    runtime: Option<Arc<MultiCoreRuntime>>,
    baseline_fds: usize,
}

impl QuiescenceGuard {
    /// The fd baseline is captured before the runtime is created, so every
    /// descriptor the runtime will ever own lies inside the measured window.
    pub fn start(workers: Option<usize>) -> Self {
        Self {
            runtime: Some(MultiCoreRuntime::new(workers).expect("quiescence runtime")),
            baseline_fds: open_fd_count(),
        }
    }

    pub fn runtime(&self) -> &Arc<MultiCoreRuntime> {
        self.runtime.as_ref().expect("quiescence guard is active")
    }

    pub fn finish_quiescent(mut self, context: &'static str) {
        let runtime = self.runtime.take().expect("quiescence guard is active");
        runtime
            .shutdown_with_timeout(SHUTDOWN_TIMEOUT)
            .expect("shutdown request");
        runtime.join().expect("quiescence workers joined");
        let snapshot = runtime
            .termination_snapshot()
            .expect("termination snapshot");
        assert!(
            snapshot.all_zero(),
            "{context}: non-zero snapshot: {snapshot:?}"
        );
        drop(runtime);
        // Settle window: sibling tests may transiently hold fds; this runtime
        // is fully dropped, so persistent growth is a leak and transient
        // growth falls back under the bar.
        let deadline = Instant::now() + SETTLE_WINDOW;
        let mut after = open_fd_count();
        while after > self.baseline_fds + FD_TOLERANCE && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            after = open_fd_count();
        }
        assert!(
            after <= self.baseline_fds + FD_TOLERANCE,
            "{context}: open fd count grew {} -> {}",
            self.baseline_fds,
            after
        );
    }
}

impl Drop for QuiescenceGuard {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
            let _ = runtime.join();
        }
    }
}

/// `assert_quiescent!(workers, "context", |runtime| { ... });` — run a body
/// against a fresh runtime, then apply the full shutdown-quiescence oracle.
/// `runtime` binds as `&Arc<MultiCoreRuntime>`. Requires the including file
/// to expose this module as `mod quiescent`.
#[allow(unused_macros)] // used by some test binaries, not all
macro_rules! assert_quiescent {
    ($workers:expr, $context:expr, |$runtime:ident| $body:block) => {{
        let guard = $crate::quiescent::QuiescenceGuard::start($workers);
        let $runtime: &std::sync::Arc<rust_miniss::MultiCoreRuntime> = guard.runtime();
        $body
        guard.finish_quiescent($context);
    }};
}

#[allow(unused_imports)]
pub(crate) use assert_quiescent;
