//! Provided buffer ring (`IORING_REGISTER_PBUF_RING`) for io_uring multishot
//! receive.
//!
//! # Kernel contract
//!
//! Mirrors `io_uring_register_buf_ring(3)` and `io_uring/kbuf.c`:
//!
//! - `IORING_REGISTER_PBUF_RING` (register opcode 25; unregister opcode 26)
//!   binds an array of `io_uring_buf` entries to a buffer group id (`bgid`).
//!   Available since kernel 5.19; on older kernels registration fails and the
//!   caller degrades to the single-shot receive path (see
//!   `UringBackend::new_with_options`).
//! - Each entry is 16 bytes (`include/uapi/linux/io_uring.h`
//!   `struct io_uring_buf`): `__u64 addr` at offset 0, `__u32 len` at 8,
//!   `__u16 bid` at 12, `__u16 resv` at 14. The `io-uring` crate exposes this
//!   layout as [`io_uring::types::BufRingEntry`], which this module uses.
//! - The `resv` field of entry 0 doubles as the shared ring **tail**. The
//!   kernel owns the head; user space publishes buffers at
//!   `tail & (count - 1)` and then bumps the tail. Publication uses a release
//!   store so the kernel observes the entry contents before the new tail.
//! - When a multishot receive selects a buffer, the kernel hands the whole
//!   buffer to the consumer: the CQE reports the byte count in `cqe.res` and
//!   the buffer id in the upper 16 bits of `cqe.flags`
//!   (`IORING_CQE_F_BUFFER`). The buffer belongs to user space again once the
//!   CQE is consumed; this module republishes it at the tail so the kernel can
//!   reuse it.
//!
//! The classic (non-`IOU_PBUF_RING_INC`) mode is used: every delivered CQE
//! consumes its whole ring slot, and the payload length comes from the CQE.
//!
//!
//! # Architecture audit
//!
//! Shard-local state only: the mappings are owned by the `UringBackend` that
//! created them, accessed exclusively from the owning worker thread, and never
//! shared. No locks, no cross-thread atomics (the tail `AtomicU16` exists for
//! the *kernel's* benefit), no runtime creation, and no blocking calls beyond
//! the two `mmap`s at construction and their `munmap`s at teardown.

use std::cell::Cell;
use std::io;
use std::sync::atomic::{AtomicU16, Ordering};

use crate::io::MULTISHOT_BUF_GROUP;

use io_uring::types::BufRingEntry;
use io_uring::IoUring;

// `MULTISHOT_BUF_GROUP` lives in `crate::io` so the net layer can name it on
// every platform; a single group per backend is sufficient because each
// multishot arm selects from the same pool and the ring is refilled as
// completions are consumed.

/// One provided buffer ring owned by a `UringBackend`.
///
/// Layout: `count` kernel-visible `io_uring_buf` entries in a page-aligned
/// mapping, plus one contiguous data region where buffer `bid` lives at
/// `data + bid * buf_size`. The id -> pointer mapping is therefore arithmetic,
/// O(1), and fixed at registration time.
pub(crate) struct BufRing {
    /// Page-aligned `io_uring_buf` entries array (kernel-visible ring).
    ring_ptr: *mut BufRingEntry,
    ring_bytes: usize,
    /// Contiguous data region backing every buffer id.
    data_ptr: *mut u8,
    data_bytes: usize,
    count: u16,
    buf_size: usize,
    /// User-space owned tail, mirrored into entry 0's `resv` field.
    tail: Cell<u16>,
}

// SAFETY: the raw pointers point at mmap'd regions exclusively owned by this
// backend. `BufRing` is only ever stored inside `UringBackend`, which is
// `!Send + !Sync` and used from its owning thread; the kernel accesses the
// ring asynchronously, which is the documented io_uring ownership model.
unsafe impl Send for BufRing {}

impl std::fmt::Debug for BufRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufRing")
            .field("count", &self.count)
            .field("buf_size", &self.buf_size)
            .field("tail", &self.tail.get())
            .finish_non_exhaustive()
    }
}

impl BufRing {
    /// Map, initialize, and register one provided buffer ring.
    ///
    /// `count` must be a non-zero power of two (the tail slot is masked).
    /// Returns the ring with every buffer published and ready for kernel
    /// selection.
    pub(crate) fn create(ring: &IoUring, count: u16, buf_size: usize) -> io::Result<Self> {
        if count == 0 || !count.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer ring count must be a non-zero power of two",
            ));
        }
        if buf_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer ring buffer size must be non-zero",
            ));
        }
        // Ring entries: `count` x 16 bytes, page-aligned because entry 0's
        // `resv` field is the kernel-shared tail and must stay mapped while
        // the kernel concurrently reads it.
        let entry_size = std::mem::size_of::<BufRingEntry>();
        let ring_bytes = round_to_page(count as usize * entry_size);
        let ring_ptr = mmap(ring_bytes)?.cast::<BufRingEntry>();
        // Data region: lazily faulted anonymous pages, one buffer per id.
        let data_bytes = round_to_page(count as usize * buf_size);
        let data_ptr = match mmap(data_bytes) {
            Ok(ptr) => ptr,
            Err(error) => {
                // SAFETY: `ring_ptr` was mapped above and is released here.
                unsafe { munmap(ring_ptr.cast(), ring_bytes) };
                return Err(error);
            }
        };

        let ring_state = Self {
            ring_ptr,
            ring_bytes,
            data_ptr,
            data_bytes,
            count,
            buf_size,
            tail: Cell::new(0),
        };
        ring_state.publish_all();
        // SAFETY: `ring_addr` points at the page-aligned, live mapping that
        // backs exactly `count` entries; it stays mapped until this backend
        // unregisters the group (see `UringBackend::drop`) or closes the ring.
        let registration = unsafe {
            ring.submitter().register_buf_ring_with_flags(
                ring_state.ring_addr(),
                count,
                MULTISHOT_BUF_GROUP,
                0,
            )
        };
        if let Err(error) = registration {
            drop(ring_state); // unmaps both regions before reporting failure
            return Err(error);
        }
        Ok(ring_state)
    }

    /// Buffer count — asserted by the registration tests.
    #[cfg(test)]
    pub(crate) fn count(&self) -> u16 {
        self.count
    }

    /// Per-buffer size — asserted by the registration tests.
    #[cfg(test)]
    pub(crate) fn buf_size(&self) -> usize {
        self.buf_size
    }

    /// Base address of the kernel-visible ring entries array.
    fn ring_addr(&self) -> u64 {
        self.ring_ptr as u64
    }

    /// Resolve a buffer id to its `(pointer, capacity)` region.
    ///
    /// Returns `None` for ids outside the registered count — a corrupted CQE
    /// must not turn into an out-of-bounds pointer.
    pub(crate) fn buffer_region(&self, bid: u16) -> Option<(*const u8, usize)> {
        if bid >= self.count {
            return None;
        }
        // SAFETY: `bid < count` bounds the offset inside the `data_bytes`
        // mapping (which is page-rounded up from `count * buf_size`).
        let ptr = unsafe { self.data_ptr.add(bid as usize * self.buf_size) };
        Some((ptr, self.buf_size))
    }

    /// Publish every buffer once, in id order, and arm the tail.
    ///
    /// Called exactly once after mapping: the ring starts full so the first
    /// multishot receive can select from id 0.
    fn publish_all(&self) {
        for bid in 0..self.count {
            self.write_entry(bid);
            self.tail.set(self.tail.get().wrapping_add(1));
        }
        self.store_tail();
    }

    /// Return one consumed buffer to the kernel's selection pool.
    ///
    /// The standard refill pattern: write the entry at the current tail slot
    /// and advance the tail by one with a release store.
    pub(crate) fn publish(&self, bid: u16) {
        debug_assert!(bid < self.count, "buffer id must be inside the ring");
        self.write_entry(bid);
        self.tail.set(self.tail.get().wrapping_add(1));
        self.store_tail();
    }

    /// Write `{addr, len, bid}` for `bid` at the current tail slot.
    fn write_entry(&self, bid: u16) {
        let mask = self.count - 1;
        let slot = (self.tail.get() & mask) as usize;
        // SAFETY: `slot < count` and the mapping holds `count` entries.
        let entry = unsafe { &mut *self.ring_ptr.add(slot) };
        let addr = self.data_ptr as u64 + bid as u64 * self.buf_size as u64;
        entry.set_addr(addr);
        entry.set_len(self.buf_size as u32);
        entry.set_bid(bid);
    }

    /// Release-store the local tail into entry 0's `resv` field.
    fn store_tail(&self) {
        // SAFETY: `ring_ptr` is the base of the live entries array, which is
        // exactly the pointer contract `BufRingEntry::tail` documents; the
        // mapping outlives this call.
        let tail_ptr = unsafe { BufRingEntry::tail(self.ring_ptr) as *mut u16 };
        // SAFETY: the tail address is u16-aligned and inside the live mapping;
        // only this thread writes it, and the kernel reads it concurrently.
        let atomic = unsafe { AtomicU16::from_ptr(tail_ptr) };
        atomic.store(self.tail.get(), Ordering::Release);
    }
}

impl Drop for BufRing {
    fn drop(&mut self) {
        // SAFETY: both mappings were created by `create` and are unmapped
        // exactly once here. Unregistration happens in `UringBackend::drop`
        // while the ring fd is still open, before this drop glue runs.
        unsafe {
            munmap(self.ring_ptr.cast(), self.ring_bytes);
            munmap(self.data_ptr, self.data_bytes);
        }
        self.ring_ptr = std::ptr::null_mut();
        self.data_ptr = std::ptr::null_mut();
    }
}

fn mmap(len: usize) -> io::Result<*mut u8> {
    // SAFETY: anonymous private mapping with no fd; the returned region is
    // owned exclusively by this module until `munmap`.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(ptr.cast::<u8>())
    }
}

/// # Safety
///
/// `ptr` must be a mapping returned by [`mmap`] with the same `len`, and must
/// not be unmapped twice.
unsafe fn munmap(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        // SAFETY: caller contract above.
        unsafe { libc::munmap(ptr.cast(), len) };
    }
}

fn round_to_page(len: usize) -> usize {
    let page = page_size();
    len.div_ceil(page) * page
}

fn page_size() -> usize {
    // SAFETY: sysconf(_SC_PAGESIZE) has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tail field is entry 0 offset 14 — assert the layout once so the
    /// `resv`-as-tail contract cannot silently drift between the crate type
    /// and the uapi definition this module documents.
    #[test]
    fn buf_ring_entry_tail_is_resv_field_of_entry_zero() {
        // SAFETY: `BufRingEntry` is a transparent wrapper over the uapi
        // `io_uring_buf`; reading its documented 16-byte layout through the
        // raw fields is valid for a live local array.
        let entries: [BufRingEntry; 1] = unsafe { std::mem::zeroed() };
        // SAFETY: `tail` points into the live local array above.
        let tail_ptr = unsafe { BufRingEntry::tail(entries.as_ptr()) };
        let base = entries.as_ptr() as usize;
        assert_eq!(
            tail_ptr as usize - base,
            14,
            "tail must alias entry 0's resv field (offset 14)"
        );
    }

    #[test]
    fn buffer_region_rejects_out_of_range_ids() {
        // Construct without registration: mmap only, no kernel involved.
        let count = 4u16;
        let buf_size = 64usize;
        let ring_bytes = round_to_page(count as usize * std::mem::size_of::<BufRingEntry>());
        let data_bytes = round_to_page(count as usize * buf_size);
        // SAFETY: mappings are owned by this test and released by `ring`.
        let ring_ptr = mmap(ring_bytes).expect("mmap ring").cast::<BufRingEntry>();
        let data_ptr = mmap(data_bytes).expect("mmap data");
        let ring = BufRing {
            ring_ptr,
            ring_bytes,
            data_ptr,
            data_bytes,
            count,
            buf_size,
            tail: Cell::new(0),
        };
        let (first, size) = ring.buffer_region(0).expect("bid 0");
        assert_eq!(size, buf_size);
        let (second, _) = ring.buffer_region(1).expect("bid 1");
        assert_eq!(
            second as usize - first as usize,
            buf_size,
            "buffer ids map to contiguous regions"
        );
        assert!(ring.buffer_region(count).is_none());
        assert!(ring.buffer_region(u16::MAX).is_none());
        drop(ring);
    }

    #[test]
    fn publish_walks_tail_slots_in_masked_order() {
        let count = 4u16;
        let buf_size = 64usize;
        let ring_bytes = round_to_page(count as usize * std::mem::size_of::<BufRingEntry>());
        let data_bytes = round_to_page(count as usize * buf_size);
        // SAFETY: mappings are owned by this test and released by `ring`.
        let ring_ptr = mmap(ring_bytes).expect("mmap ring").cast::<BufRingEntry>();
        let data_ptr = mmap(data_bytes).expect("mmap data");
        let ring = BufRing {
            ring_ptr,
            ring_bytes,
            data_ptr,
            data_bytes,
            count,
            buf_size,
            tail: Cell::new(0),
        };
        ring.publish_all();
        assert_eq!(ring.tail.get(), count);
        // Republishing wraps through the masked slots without disturbing ids.
        for bid in 0..count {
            ring.publish(bid);
        }
        assert_eq!(ring.tail.get(), count * 2);
        // Entry at (tail & mask) for bid 0 must carry bid 0's address.
        // SAFETY: slot 0 is inside the live mapping.
        let entry = unsafe { &*ring_ptr };
        assert_eq!(entry.bid(), 0);
        assert_eq!(
            entry.addr(),
            data_ptr as u64,
            "bid 0 must map to the base of the data region"
        );
        drop(ring);
    }

    #[test]
    fn create_rejects_invalid_parameters_before_touching_the_ring() {
        // Validation runs before any syscall touches the fd, so an invalid
        // fd (here: the unconstructable -1) never gets a syscall issued.
        // `IoUring` cannot be faked, so exercise only the parameter checks by
        // ensuring the count/size errors fire before registration is reached:
        // construct with a real ring and assert the error kind.
        let ring = io_uring::IoUring::new(2).expect("io_uring must be available");
        let error = BufRing::create(&ring, 3, 4096).expect_err("non-power-of-two count");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = BufRing::create(&ring, 0, 4096).expect_err("zero count");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = BufRing::create(&ring, 4, 0).expect_err("zero buffer size");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
