//! Shared-memory bus: an mmap'd file holding a header + two SPSC rings.
//!
//! The header carries a magic + version so a mis-matched build can't silently
//! attach to an older layout. Both processes call `ShmBus::open_or_create`;
//! whoever observes a fresh (zeroed) file initialises the rings, the other
//! side just attaches.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use memmap2::{MmapMut, MmapOptions};

use crate::ring::{RingConsumer, RingProducer, SpscRing};

pub const BUS_MAGIC: u32 = 0x5348_4D42; // "SHMB"
pub const BUS_VERSION: u32 = 1;

#[repr(C, align(64))]
struct BusHeader {
    magic: AtomicU32,
    version: AtomicU32,
    ready: AtomicU32, // set to 1 once rings are initialised
    _pad: [u8; 64 - 12],
}

#[repr(C, align(64))]
struct BusLayout {
    header: BusHeader,
    ticks: SpscRing,
    orders: SpscRing,
}

pub struct ShmBus {
    _mmap: MmapMut,
    layout: *mut BusLayout,
}

unsafe impl Send for ShmBus {}
unsafe impl Sync for ShmBus {}

impl ShmBus {
    /// Open (or create + initialise) the shared-memory bus at `path`.
    pub fn open_or_create(path: impl AsRef<Path>) -> io::Result<Self> {
        let size = std::mem::size_of::<BusLayout>();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(path.as_ref())?;
        let existing_len = file.metadata()?.len();
        let fresh = existing_len < size as u64;
        if fresh {
            file.set_len(size as u64)?;
        }

        // Safety: file is the right size and we own the mapping for our lifetime.
        let mut mmap = unsafe { MmapOptions::new().len(size).map_mut(&file)? };
        let layout = mmap.as_mut_ptr() as *mut BusLayout;

        unsafe {
            let hdr = &(*layout).header;
            if fresh || hdr.magic.load(Ordering::Acquire) != BUS_MAGIC {
                // Cold init. Zero the region (mmap of a freshly-extended file
                // is already zeroed, but be explicit) and set up the rings.
                std::ptr::write_bytes(layout as *mut u8, 0, size);
                (*layout).ticks.init_in_place();
                (*layout).orders.init_in_place();
                hdr.version.store(BUS_VERSION, Ordering::Relaxed);
                hdr.magic.store(BUS_MAGIC, Ordering::Release);
                hdr.ready.store(1, Ordering::Release);
            } else {
                let ver = hdr.version.load(Ordering::Acquire);
                if ver != BUS_VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("shm bus version mismatch: file={ver}, code={BUS_VERSION}"),
                    ));
                }
                // Spin briefly for the creator's ready bit.
                while hdr.ready.load(Ordering::Acquire) != 1 {
                    std::hint::spin_loop();
                }
            }
        }

        Ok(Self { _mmap: mmap, layout })
    }

    fn ticks_ring(&self) -> *const SpscRing {
        unsafe { &(*self.layout).ticks as *const _ }
    }

    fn orders_ring(&self) -> *const SpscRing {
        unsafe { &(*self.layout).orders as *const _ }
    }

    /// Producer side of the ticks ring (used by the feedhandler).
    ///
    /// # Safety
    /// At most one producer for this ring across all processes.
    pub unsafe fn ticks_producer(&self) -> RingProducer {
        RingProducer::new(self.ticks_ring())
    }

    /// Consumer side of the ticks ring (used by the trading engine).
    ///
    /// # Safety
    /// At most one consumer for this ring across all processes.
    pub unsafe fn ticks_consumer(&self) -> RingConsumer {
        RingConsumer::new(self.ticks_ring())
    }

    /// Producer side of the orders ring (trading engine -> feedhandler).
    ///
    /// # Safety
    /// At most one producer for this ring across all processes.
    pub unsafe fn orders_producer(&self) -> RingProducer {
        RingProducer::new(self.orders_ring())
    }

    /// Consumer side of the orders ring.
    ///
    /// # Safety
    /// At most one consumer for this ring across all processes.
    pub unsafe fn orders_consumer(&self) -> RingConsumer {
        RingConsumer::new(self.orders_ring())
    }
}
