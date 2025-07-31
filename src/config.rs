//! Configuration constants for the Miniss runtime
//! 
//! This module contains tunable parameters that affect runtime behavior,
//! particularly around back-pressure and resource management.

/// Channel capacity for cross-CPU communication
/// 
/// This controls the bounded channel size used for sending messages between CPUs.
/// A larger value provides more buffering but uses more memory, while a smaller
/// value provides more back-pressure but may cause blocking under high load.
/// 
/// The default value of 1000 should be sufficient for most workloads while
/// providing adequate back-pressure when needed.
pub const CROSS_CPU_CHANNEL_CAPACITY: usize = 1000;

/// Default timeout for CPU thread operations (in milliseconds)
/// 
/// This controls how long CPU threads will wait for new messages when the
/// ready queue is empty. A smaller value provides more responsive shutdown
/// but uses more CPU cycles, while a larger value reduces CPU usage but
/// may delay shutdown.
pub const CPU_THREAD_TIMEOUT_MS: u64 = 10;
