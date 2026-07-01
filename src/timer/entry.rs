use std::task::Waker;

/// Represents an entry in the TimerWheel.
pub struct Entry {
    pub waker: Waker,
    pub remaining_rounds: u64,
}
