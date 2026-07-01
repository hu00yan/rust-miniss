use std::sync::atomic::{AtomicU64, Ordering};

/// A unique identifier for a timer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

static TIMER_COUNTER: AtomicU64 = AtomicU64::new(1);

const SLOT_BITS: u64 = 20;
const KEY_BITS: u64 = 20;
const GEN_BITS: u64 = 24;

const KEY_MASK: u64 = (1 << KEY_BITS) - 1;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;
const GEN_MASK: u64 = (1 << GEN_BITS) - 1;

impl Default for TimerId {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerId {
    /// Generates a new unique TimerId
    pub fn new() -> Self {
        TimerId(TIMER_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) fn from_parts(slot: usize, key: usize, generation: u32) -> Self {
        assert!(slot <= SLOT_MASK as usize, "timer slot overflow");
        assert!(key <= KEY_MASK as usize, "timer key overflow");
        assert!((generation as u64) <= GEN_MASK, "timer generation overflow");

        let raw = ((generation as u64) << (SLOT_BITS + KEY_BITS))
            | ((slot as u64) << KEY_BITS)
            | (key as u64);
        TimerId(raw)
    }

    pub(crate) fn parts(self) -> (usize, usize, u32) {
        let key = (self.0 & KEY_MASK) as usize;
        let slot = ((self.0 >> KEY_BITS) & SLOT_MASK) as usize;
        let generation = ((self.0 >> (SLOT_BITS + KEY_BITS)) & GEN_MASK) as u32;
        (slot, key, generation)
    }
}
