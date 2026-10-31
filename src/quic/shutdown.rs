//! A3: shutdown bridge (wt-quic-audit A3; methodology §T4 INV-6, S2/D28).
//!
//! The runtime's shutdown path does not know about quinn endpoints, and
//! quinn's `Runtime::spawn` cannot report admission failure — a lost
//! endpoint driver currently dies silently (`src/quic.rs:52-62`). This
//! bridge makes endpoint shutdown an explicit, bounded lifecycle step:
//!
//! - `QuicEndpointGuard` owns an endpoint; dropping it closes every
//!   connection (code 0) and posts a bounded drain task whose outcome is
//!   published on a shared `DrainStatus` the owner can observe after the
//!   drop.
//! - `QuicEndpointGuard::drained()` is the strong form: close plus a
//!   bounded `wait_idle`, resolving `DrainError::TimedOut` when the drain
//!   cannot finish — which is exactly the shape of a lost or stuck
//!   endpoint driver. The silent driver loss becomes a bounded, visible
//!   lifecycle error instead of an endpoint that dies quietly.
//!
//! No locks, no runtime creation, no cross-core traffic beyond the
//! runtime's own bounded spawn routing; the drain runs on whatever shard
//! the runtime routes it to and publishes its outcome through one
//! `AtomicU8` status flag (the existing quic.rs flag pattern).

use quinn::Endpoint;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Observable drain lifecycle. `AtomicU8` follows the existing quic.rs
/// flag pattern: one control-plane status word, written by the drain task,
/// read by anyone after the fact.
#[derive(Debug, Default)]
pub struct DrainStatus {
    state: AtomicU8,
}

/// Drain lifecycle values (`DrainStatus::state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainState {
    /// The guard exists; no drain has been requested yet.
    Open,
    /// `close` has been issued; the bounded drain is in flight.
    Closing,
    /// The endpoint reached idle within the bound.
    Drained,
    /// The drain did not finish within the bound: the endpoint driver is
    /// lost or stuck, or the runtime shut down underneath the drain.
    TimedOut,
}

impl DrainState {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Closing => 1,
            Self::Drained => 2,
            Self::TimedOut => 3,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Closing,
            2 => Self::Drained,
            3 => Self::TimedOut,
            _ => Self::Open,
        }
    }
}

impl DrainStatus {
    /// The current lifecycle state.
    pub fn state(&self) -> DrainState {
        DrainState::from_u8(self.state.load(Ordering::Acquire))
    }

    fn set(&self, state: DrainState) {
        self.state.store(state.as_u8(), Ordering::Release);
    }
}

/// Why a bounded drain failed.
#[derive(Debug, Error)]
pub enum DrainError {
    /// The endpoint did not reach idle within the guard's bound. This is
    /// the bounded, visible form of the silent driver-loss race: without
    /// the bound, a lost or stuck endpoint driver would hang the drain
    /// forever and the endpoint would die silently.
    #[error("QUIC endpoint drain did not complete within {timeout:?}: the endpoint driver is lost or stuck")]
    TimedOut { timeout: Duration },
}

/// RAII owner of one quinn endpoint with a bounded shutdown drain.
///
/// Construct it where the endpoint is created (the endpoint's owning
/// shard); drop it there or anywhere — `Drop` closes every connection
/// immediately and hands the bounded `wait_idle` drain to the runtime's
/// spawn routing, publishing the outcome on the shared [`DrainStatus`]
/// from `status()`.
pub struct QuicEndpointGuard {
    endpoint: Option<Endpoint>,
    runtime: Arc<crate::multicore::MultiCoreRuntime>,
    drain_timeout: Duration,
    status: Arc<DrainStatus>,
}

impl std::fmt::Debug for QuicEndpointGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicEndpointGuard")
            .field("drain_timeout", &self.drain_timeout)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl QuicEndpointGuard {
    /// Wrap an endpoint with the bounded shutdown drain.
    pub fn new(
        endpoint: Endpoint,
        runtime: Arc<crate::multicore::MultiCoreRuntime>,
        drain_timeout: Duration,
    ) -> Self {
        Self {
            endpoint: Some(endpoint),
            runtime,
            drain_timeout,
            status: Arc::new(DrainStatus::default()),
        }
    }

    /// The shared drain lifecycle, observable after `Drop`.
    pub fn status(&self) -> Arc<DrainStatus> {
        Arc::clone(&self.status)
    }

    /// The endpoint reference for serving (accept loops, acceptors).
    pub fn endpoint(&self) -> &Endpoint {
        self.endpoint
            .as_ref()
            .expect("endpoint is only taken in Drop")
    }

    /// The strong drain: close every connection and wait, bounded by the
    /// guard's timeout, until the endpoint is idle.
    ///
    /// This is the visibility contract for the silent driver-loss race:
    /// if the endpoint driver was lost (its task never admitted) or is
    /// stuck, `wait_idle` cannot complete and this returns
    /// [`DrainError::TimedOut`] within the bound instead of hanging.
    pub async fn drained(&self) -> Result<(), DrainError> {
        let endpoint = self.endpoint();
        self.status.set(DrainState::Closing);
        endpoint.close(0u32.into(), b"guarded shutdown");
        match crate::timer::timeout(self.drain_timeout, endpoint.wait_idle()).await {
            Ok(()) => {
                self.status.set(DrainState::Drained);
                Ok(())
            }
            Err(_timeout) => {
                self.status.set(DrainState::TimedOut);
                Err(DrainError::TimedOut {
                    timeout: self.drain_timeout,
                })
            }
        }
    }
}

impl Drop for QuicEndpointGuard {
    fn drop(&mut self) {
        let Some(endpoint) = self.endpoint.take() else {
            return;
        };
        // Close immediately: the CONNECTION_CLOSE goes out synchronously.
        self.status.set(DrainState::Closing);
        endpoint.close(0u32.into(), b"guarded shutdown");
        // The bounded drain continues in the background; its outcome is
        // visible on the shared status even though `drop` cannot await.
        let runtime = Arc::clone(&self.runtime);
        let drain_status = Arc::clone(&self.status);
        let failure_status = Arc::clone(&self.status);
        let drain_timeout = self.drain_timeout;
        let drain = async move { bounded_drain(endpoint, drain_status, drain_timeout).await };
        let spawned = if crate::cpu::current_core_id().is_some() {
            runtime.spawn_local(drain)
        } else {
            runtime.spawn_detached(drain)
        };
        if spawned.is_err() {
            // The runtime is gone: no drain can run. Publish the bound's
            // expiry shape so the loss is visible instead of silent.
            failure_status.set(DrainState::TimedOut);
        }
    }
}

/// Bounded `wait_idle` for the drop path; publishes the outcome.
async fn bounded_drain(endpoint: Endpoint, status: Arc<DrainStatus>, drain_timeout: Duration) {
    match crate::timer::timeout(drain_timeout, endpoint.wait_idle()).await {
        Ok(()) => status.set(DrainState::Drained),
        Err(_timeout) => {
            status.set(DrainState::TimedOut);
            tracing::error!(
                "QUIC endpoint drain did not complete within {drain_timeout:?}: endpoint driver \
                 lost or stuck"
            );
        }
    }
}
