use super::id::TimerId;
use std::task::Waker;

/// Represents an entry in the TimerWheel, associating a TimerId with a Waker
pub struct Entry {
    pub id: TimerId,
    pub waker: Waker,
}
