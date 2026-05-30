//! Lock-free SPSC ring buffer designed to live in shared memory.
//!
//! Layout (all C-compatible, fixed offsets so two processes agree):
//!
//! ```text
//!   +--------------------------------+
//!   | head  (cacheline padded, u64)  |  written by producer
//!   +--------------------------------+
//!   | tail  (cacheline padded, u64)  |  written by consumer
//!   +--------------------------------+
//!   | slots[CAPACITY]                |
//!   |   each slot:                   |
//!   |     seq:    u64 (atomic)       |
//!   |     _pad:   u64                |
//!   |     payload [u8; SLOT_PAYLOAD] |
//!   +--------------------------------+
//! ```
//!
//! `seq` doubles as a "ready" flag. A producer writes `slot.seq = head + 1`
//! *after* the payload bytes are in place; the consumer waits until `seq`
//! equals its expected value before reading. This avoids needing a separate
//! commit-barrier word.

use std::sync::atomic::{AtomicU64, Ordering};

/// Power-of-two slot count. 16k * 96B per slot ≈ 1.5 MiB per ring.
pub const RING_CAPACITY: usize = 16 * 1024;
const _: () = assert!(RING_CAPACITY.is_power_of_two());
pub const RING_MASK: u64 = (RING_CAPACITY as u64) - 1;

/// Bytes available per slot for application payload.
pub const SLOT_PAYLOAD: usize = 80;
const CACHELINE: usize = 64;

#[repr(C, align(64))]
struct PaddedAtomic {
    v: AtomicU64,
    _pad: [u8; CACHELINE - 8],
}

#[repr(C, align(64))]
pub struct Slot {
    pub seq: AtomicU64,
    pub _pad: u64,
    pub payload: [u8; SLOT_PAYLOAD],
}

/// Shared-memory ring buffer header + slots, fixed size.
#[repr(C, align(64))]
pub struct SpscRing {
    head: PaddedAtomic, // producer cursor (next sequence to publish)
    tail: PaddedAtomic, // consumer cursor (next sequence to consume)
    pub slots: [Slot; RING_CAPACITY],
}

impl SpscRing {
    /// Reset all atomics & slots. Must be called by exactly one process
    /// before the bus goes live.
    pub fn init_in_place(&self) {
        self.head.v.store(0, Ordering::Relaxed);
        self.tail.v.store(0, Ordering::Relaxed);
        for slot in self.slots.iter() {
            slot.seq.store(0, Ordering::Relaxed);
        }
    }

    pub fn capacity(&self) -> usize {
        RING_CAPACITY
    }
}

/// SPSC producer handle. Not `Sync` — only one producer per ring.
pub struct RingProducer {
    ring: *const SpscRing,
    next_seq: u64,
}

unsafe impl Send for RingProducer {}

impl RingProducer {
    /// # Safety
    /// Caller must ensure `ring` points at a properly initialised `SpscRing`
    /// in shared memory and that this is the *only* producer.
    pub unsafe fn new(ring: *const SpscRing) -> Self {
        let next_seq = (*ring).head.v.load(Ordering::Relaxed);
        Self { ring, next_seq }
    }

    /// Try to publish `bytes` (≤ SLOT_PAYLOAD). Returns false if the ring is full.
    pub fn try_publish(&mut self, bytes: &[u8]) -> bool {
        assert!(bytes.len() <= SLOT_PAYLOAD);
        // Safety: ring pointer was validated at construction. The reference
        // does not borrow `self` because it comes from a raw pointer (Copy).
        let ring: &SpscRing = unsafe { &*self.ring };
        let head = self.next_seq;
        let tail = ring.tail.v.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= RING_CAPACITY as u64 {
            return false; // full
        }
        let slot = &ring.slots[(head & RING_MASK) as usize];

        // Copy payload before publishing seq.
        // Safety: we own this slot until we bump seq.
        unsafe {
            let dst = slot.payload.as_ptr() as *mut u8;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            if bytes.len() < SLOT_PAYLOAD {
                std::ptr::write_bytes(dst.add(bytes.len()), 0, SLOT_PAYLOAD - bytes.len());
            }
        }

        // Release: payload writes happen-before seq update visible to consumer.
        slot.seq.store(head + 1, Ordering::Release);
        self.next_seq = head + 1;
        ring.head.v.store(self.next_seq, Ordering::Release);
        true
    }
}

/// SPSC consumer handle.
pub struct RingConsumer {
    ring: *const SpscRing,
    next_seq: u64,
}

unsafe impl Send for RingConsumer {}

impl RingConsumer {
    /// # Safety
    /// Caller must ensure `ring` points at a properly initialised `SpscRing`
    /// in shared memory and that this is the *only* consumer.
    pub unsafe fn new(ring: *const SpscRing) -> Self {
        let next_seq = (*ring).tail.v.load(Ordering::Relaxed);
        Self { ring, next_seq }
    }

    /// Try to consume one message into `out` (which must be ≥ SLOT_PAYLOAD bytes).
    /// Returns the number of bytes copied (always SLOT_PAYLOAD) or None if empty.
    pub fn try_consume(&mut self, out: &mut [u8]) -> Option<usize> {
        assert!(out.len() >= SLOT_PAYLOAD);
        // Safety: raw-pointer deref, see RingProducer::try_publish.
        let ring: &SpscRing = unsafe { &*self.ring };
        let expected = self.next_seq + 1;
        let slot = &ring.slots[(self.next_seq & RING_MASK) as usize];
        let seq = slot.seq.load(Ordering::Acquire);
        if seq != expected {
            return None;
        }
        // Safety: producer published this slot.
        unsafe {
            std::ptr::copy_nonoverlapping(
                slot.payload.as_ptr(),
                out.as_mut_ptr(),
                SLOT_PAYLOAD,
            );
        }
        self.next_seq = expected;
        ring.tail.v.store(self.next_seq, Ordering::Release);
        Some(SLOT_PAYLOAD)
    }

    /// Approximate number of unread messages (snapshot — may race).
    pub fn pending(&self) -> u64 {
        let ring: &SpscRing = unsafe { &*self.ring };
        let head = ring.head.v.load(Ordering::Acquire);
        head.saturating_sub(self.next_seq)
    }
}
