use super::id::TimerId;
use std::task::Waker;

/// Represents an entry in the TimerWheel, associating a TimerId with a Waker
/// and tracking how many full rotations remain before expiration.
pub struct Entry {
    pub id: TimerId,
    pub waker: Waker,
    pub remaining_rounds: u64,
}
